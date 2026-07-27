use crate::backend::{
    BackendError, BackendInfo, BackendKind, Batch, BatchScan, BatchScanner, MiningKey, block_lanes,
    padded_rate_block,
};
use crate::create2::{create2_digest_from_hash, salt_from_counter};
use bytemuck::{Pod, Zeroable};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, mpsc};
use wgpu::util::DeviceExt;

const WORKGROUP_SIZE: u32 = 64;
const RESULT_WORDS: usize = 9;
const RESULT_SIZE: u64 = (RESULT_WORDS * std::mem::size_of::<u32>()) as u64;
const NO_MATCH: u32 = u32::MAX;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuParams {
    start_counter: u64,
    count: u32,
    tile_start: u32,
    lanes: [u64; 17],
    mask: [u32; 5],
    value: [u32; 5],
}

pub(crate) struct GpuScanner {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    bind_group: wgpu::BindGroup,
    params_buffer: wgpu::Buffer,
    result_buffer: wgpu::Buffer,
    readback_buffer: wgpu::Buffer,
    params: GpuParams,
    max_workgroups_per_dispatch: u32,
    info: BackendInfo,
    uncaptured_error: Arc<Mutex<Option<String>>>,
}

impl GpuScanner {
    pub(crate) fn new(key: MiningKey) -> Result<Self, BackendError> {
        pollster::block_on(Self::new_async(key))
    }

    async fn new_async(key: MiningKey) -> Result<Self, BackendError> {
        let backends = native_backends();
        let mut instance_descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_descriptor.backends = backends;
        let instance = wgpu::Instance::new(instance_descriptor);
        let adapters = instance.enumerate_adapters(backends).await;
        let diagnostics = adapters
            .iter()
            .map(adapter_diagnostic)
            .collect::<Vec<_>>()
            .join("; ");
        let mut compatible = adapters
            .into_iter()
            .filter(adapter_is_compatible)
            .collect::<Vec<_>>();
        compatible.sort_by_key(|adapter| adapter_rank(adapter.get_info().device_type));
        let adapter = compatible.pop().ok_or_else(|| {
            BackendError::new(format!(
                "no hardware GPU supports compute workgroups of {WORKGROUP_SIZE} threads and SHADER_INT64{}",
                if diagnostics.is_empty() {
                    String::new()
                } else {
                    format!(" ({diagnostics})")
                }
            ))
        })?;

        let adapter_info = adapter.get_info();
        let adapter_limits = adapter.limits();
        let required_limits = wgpu::Limits {
            max_compute_workgroups_per_dimension: adapter_limits
                .max_compute_workgroups_per_dimension,
            ..Default::default()
        };
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("vanity CREATE2 device"),
                required_features: wgpu::Features::SHADER_INT64,
                required_limits,
                ..Default::default()
            })
            .await
            .map_err(|error| {
                BackendError::new(format!(
                    "could not create a compute device on {}: {error}",
                    adapter_info.name
                ))
            })?;

        let uncaptured_error = Arc::new(Mutex::new(None));
        let error_slot = Arc::clone(&uncaptured_error);
        device.on_uncaptured_error(Arc::new(move |error| {
            if let Ok(mut slot) = error_slot.lock() {
                *slot = Some(error.to_string());
            }
        }));

        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let shader = device.create_shader_module(wgpu::include_wgsl!("create2.wgsl"));
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("vanity CREATE2 pipeline"),
            layout: None,
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let layout = pipeline.get_bind_group_layout(0);

        let block = padded_rate_block(key);
        let params = GpuParams {
            start_counter: 0,
            count: 1,
            tile_start: 0,
            lanes: block_lanes(block),
            mask: key.mask,
            value: key.value,
        };
        let params_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vanity CREATE2 parameters"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });
        let initial_result = initial_result();
        let result_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vanity CREATE2 result"),
            contents: bytemuck::cast_slice(&initial_result),
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST,
        });
        let readback_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vanity CREATE2 readback"),
            size: RESULT_SIZE,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vanity CREATE2 bind group"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: result_buffer.as_entire_binding(),
                },
            ],
        });
        if let Some(error) = scope.pop().await {
            return Err(BackendError::new(format!(
                "GPU shader or pipeline validation failed on {}: {error}",
                adapter_info.name
            )));
        }

        let info = BackendInfo {
            kind: BackendKind::Gpu,
            adapter: Some(adapter_info.name.clone()),
            graphics_api: Some(format!("{:?}", adapter_info.backend)),
            fallback_reason: None,
        };
        let mut scanner = Self {
            device,
            queue,
            pipeline,
            bind_group,
            params_buffer,
            result_buffer,
            readback_buffer,
            params,
            max_workgroups_per_dispatch: adapter_limits.max_compute_workgroups_per_dimension,
            info,
            uncaptured_error,
        };
        scanner.self_test(key)?;
        Ok(scanner)
    }

    pub(crate) fn backend_info(&self) -> BackendInfo {
        self.info.clone()
    }

    fn self_test(&mut self, key: MiningKey) -> Result<(), BackendError> {
        let scan = self.scan(
            Batch {
                start_counter: 0,
                count: 1,
            },
            &AtomicBool::new(false),
        )?;
        let BatchScan::Complete { witness, .. } = scan else {
            return Err(BackendError::new(
                "GPU self-test was unexpectedly cancelled",
            ));
        };
        let expected =
            create2_digest_from_hash(key.deployer, salt_from_counter(0), key.init_code_hash);
        if witness != expected {
            return Err(BackendError::new(format!(
                "GPU self-test digest mismatch on {}",
                self.info.summary()
            )));
        }
        Ok(())
    }

    fn dispatch_tile(&mut self, tile_count: u32) -> Result<[u32; RESULT_WORDS], BackendError> {
        self.queue
            .write_buffer(&self.params_buffer, 0, bytemuck::bytes_of(&self.params));

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vanity CREATE2 dispatch"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("vanity CREATE2 compute pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.dispatch_workgroups(tile_count.div_ceil(WORKGROUP_SIZE), 1, 1);
        }
        encoder.copy_buffer_to_buffer(
            &self.result_buffer,
            0,
            &self.readback_buffer,
            0,
            RESULT_SIZE,
        );
        self.queue.submit([encoder.finish()]);

        let slice = self.readback_buffer.slice(..);
        let (sender, receiver) = mpsc::sync_channel(1);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| BackendError::new(format!("GPU device polling failed: {error}")))?;
        receiver
            .recv()
            .map_err(|_| BackendError::new("GPU readback callback did not complete"))?
            .map_err(|error| BackendError::new(format!("GPU readback mapping failed: {error}")))?;

        let mut words = [0_u32; RESULT_WORDS];
        {
            let mapped = slice
                .get_mapped_range()
                .map_err(|error| BackendError::new(format!("GPU readback failed: {error}")))?;
            words.copy_from_slice(bytemuck::cast_slice(&mapped));
        }
        self.readback_buffer.unmap();
        if let Some(error) = self.take_uncaptured_error() {
            return Err(BackendError::new(format!("GPU device error: {error}")));
        }
        Ok(words)
    }

    fn take_uncaptured_error(&self) -> Option<String> {
        self.uncaptured_error
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }
}

impl BatchScanner for GpuScanner {
    fn scan(&mut self, batch: Batch, cancelled: &AtomicBool) -> Result<BatchScan, BackendError> {
        if batch.count == 0 {
            return Err(BackendError::new("GPU received an empty batch"));
        }
        if let Some(error) = self.take_uncaptured_error() {
            return Err(BackendError::new(format!("GPU device error: {error}")));
        }

        let initial_result = initial_result();
        self.queue.write_buffer(
            &self.result_buffer,
            0,
            bytemuck::cast_slice(&initial_result),
        );
        self.params.start_counter = batch.start_counter;
        self.params.count = batch.count;
        self.params.tile_start = 0;

        let max_tile_candidates = u64::from(self.max_workgroups_per_dispatch) * 64;
        if max_tile_candidates == 0 {
            return Err(BackendError::new(
                "GPU reports a zero compute dispatch limit",
            ));
        }

        while self.params.tile_start < batch.count {
            let remaining = u64::from(batch.count - self.params.tile_start);
            let tile_count = remaining.min(max_tile_candidates) as u32;
            let words = self.dispatch_tile(tile_count)?;

            if words[0] != NO_MATCH {
                return Ok(BatchScan::Complete {
                    match_offset: Some(words[0]),
                    witness: witness_from_words(&words[1..]),
                });
            }
            if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(BatchScan::Cancelled);
            }
            self.params.tile_start = self
                .params
                .tile_start
                .checked_add(tile_count)
                .ok_or_else(|| BackendError::new("GPU tile offset overflowed"))?;

            if self.params.tile_start == batch.count {
                return Ok(BatchScan::Complete {
                    match_offset: None,
                    witness: witness_from_words(&words[1..]),
                });
            }
        }

        Err(BackendError::new(
            "GPU batch completed without producing a result",
        ))
    }
}

fn adapter_is_compatible(adapter: &wgpu::Adapter) -> bool {
    let info = adapter.get_info();
    let limits = adapter.limits();
    info.device_type != wgpu::DeviceType::Cpu
        && adapter.features().contains(wgpu::Features::SHADER_INT64)
        && limits.max_compute_invocations_per_workgroup >= WORKGROUP_SIZE
        && limits.max_compute_workgroup_size_x >= WORKGROUP_SIZE
        && limits.max_compute_workgroups_per_dimension > 0
}

fn adapter_diagnostic(adapter: &wgpu::Adapter) -> String {
    let info = adapter.get_info();
    let limits = adapter.limits();
    format!(
        "{} ({:?}, {:?}): SHADER_INT64={}, max workgroup invocations={}, max workgroup x={}",
        info.name,
        info.device_type,
        info.backend,
        adapter.features().contains(wgpu::Features::SHADER_INT64),
        limits.max_compute_invocations_per_workgroup,
        limits.max_compute_workgroup_size_x
    )
}

const fn adapter_rank(device_type: wgpu::DeviceType) -> u8 {
    match device_type {
        wgpu::DeviceType::DiscreteGpu => 4,
        wgpu::DeviceType::IntegratedGpu => 3,
        wgpu::DeviceType::VirtualGpu => 2,
        wgpu::DeviceType::Other => 1,
        wgpu::DeviceType::Cpu => 0,
    }
}

const fn native_backends() -> wgpu::Backends {
    #[cfg(target_os = "macos")]
    {
        wgpu::Backends::METAL
    }
    #[cfg(target_os = "linux")]
    {
        wgpu::Backends::VULKAN
    }
    #[cfg(target_os = "windows")]
    {
        wgpu::Backends::DX12
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        wgpu::Backends::empty()
    }
}

const fn initial_result() -> [u32; RESULT_WORDS] {
    let mut words = [0_u32; RESULT_WORDS];
    words[0] = NO_MATCH;
    words
}

fn witness_from_words(words: &[u32]) -> [u8; 32] {
    let mut witness = [0_u8; 32];
    for (destination, word) in witness.chunks_exact_mut(4).zip(words.iter().copied()) {
        destination.copy_from_slice(&word.to_le_bytes());
    }
    witness
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendPreference, SearchEvent};
    use crate::create2::{
        Address, Create2Miner, MiningOptions, SearchOutcome, VanityPattern, create2_address,
    };

    #[test]
    fn shader_validates_and_translates_for_all_native_backends() {
        let module =
            naga::front::wgsl::parse_str(include_str!("create2.wgsl")).expect("WGSL should parse");
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("WGSL should validate");

        let (msl, _) = naga::back::msl::write_string(
            &module,
            &info,
            &naga::back::msl::Options {
                lang_version: (2, 3),
                ..Default::default()
            },
            &naga::back::msl::PipelineOptions::default(),
        )
        .expect("WGSL should translate to MSL");
        assert!(msl.contains("kernel"));

        let spirv = naga::back::spv::write_vec(
            &module,
            &info,
            &naga::back::spv::Options::default(),
            Some(&naga::back::spv::PipelineOptions {
                shader_stage: naga::ShaderStage::Compute,
                entry_point: "main".to_owned(),
            }),
        )
        .expect("WGSL should translate to SPIR-V");
        assert!(!spirv.is_empty());

        let hlsl_options = naga::back::hlsl::Options {
            shader_model: naga::back::hlsl::ShaderModel::V6_0,
            ..Default::default()
        };
        let hlsl_pipeline_options = naga::back::hlsl::PipelineOptions::default();
        let mut hlsl = String::new();
        naga::back::hlsl::Writer::new(&mut hlsl, &hlsl_options, &hlsl_pipeline_options)
            .write(&module, &info, None)
            .expect("WGSL should translate to HLSL");
        assert!(hlsl.contains("numthreads"));
    }

    #[test]
    fn result_witness_words_are_little_endian_digest_bytes() {
        let words = [0x0302_0100, 0x0706_0504, 0x0b0a_0908, 0x0f0e_0d0c];
        let witness = witness_from_words(&words);
        assert_eq!(&witness[..16], &(0_u8..16).collect::<Vec<_>>());
    }

    #[test]
    fn optional_hardware_conformance() {
        let requested = std::env::var_os("VANITY_GPU_TESTS").is_some();
        let required = std::env::var_os("VANITY_REQUIRE_GPU").is_some();
        if !requested && !required {
            return;
        }

        let deployer = Address::from_bytes([0; 20]);
        let init_code = [0_u8];
        let target_counter = 64;
        let target_address =
            create2_address(deployer, salt_from_counter(target_counter), &init_code);
        let miner = Create2Miner::new(
            deployer,
            &init_code,
            VanityPattern::new(&target_address.to_string(), "").unwrap(),
        );
        let mut session = match miner.backend_session(BackendPreference::Gpu) {
            Ok(session) => session,
            Err(error) if !required => {
                eprintln!("skipping optional GPU conformance test: {error}");
                return;
            }
            Err(error) => {
                panic!("VANITY_REQUIRE_GPU is set but no compatible GPU is usable: {error}")
            }
        };
        let outcome = miner
            .search_with_backend(
                &mut session,
                MiningOptions {
                    start_counter: 0,
                    max_attempts: Some(target_counter + 1),
                    batch_size: target_counter + 1,
                },
                &AtomicBool::new(false),
                |event| assert!(matches!(event, SearchEvent::Progress(_))),
            )
            .expect("GPU boundary search should complete");
        let SearchOutcome::Found(result) = outcome else {
            panic!("GPU should find the exact counter-64 target");
        };
        assert_eq!(result.salt, salt_from_counter(target_counter));
        assert_eq!(
            create2_address(deployer, result.salt, &init_code),
            result.address
        );

        // Deterministic pseudo-random ranges differentially compare the two
        // adapters through the public coordinator seam.
        let mut cpu_session = miner
            .backend_session(BackendPreference::Cpu)
            .expect("CPU oracle should initialize");
        let mut random_state = 0x9e37_79b9_u32;
        for _ in 0..8 {
            random_state ^= random_state << 13;
            random_state ^= random_state >> 17;
            random_state ^= random_state << 5;
            let start_counter = u64::from(random_state % 128);
            let attempts = u64::from((random_state >> 8) % 96 + 1);
            let options = MiningOptions {
                start_counter,
                max_attempts: Some(attempts),
                batch_size: 73,
            };
            let cpu_outcome = miner
                .search_with_backend(&mut cpu_session, options, &AtomicBool::new(false), |_| {})
                .expect("CPU differential search should complete");
            let gpu_outcome = miner
                .search_with_backend(&mut session, options, &AtomicBool::new(false), |_| {})
                .expect("GPU differential search should complete");
            assert_eq!(gpu_outcome, cpu_outcome);
        }

        let eip_miner = Create2Miner::new(
            deployer,
            &init_code,
            VanityPattern::new("4d1a2e2bb4f88f0250f26ffff098b0b30b26bf38", "").unwrap(),
        );
        let mut eip_session = eip_miner
            .backend_session(BackendPreference::Gpu)
            .expect("the selected GPU should remain available");
        let SearchOutcome::Found(eip_result) = eip_miner
            .search_with_backend(
                &mut eip_session,
                MiningOptions {
                    max_attempts: Some(1),
                    batch_size: 1,
                    ..MiningOptions::default()
                },
                &AtomicBool::new(false),
                |_| {},
            )
            .expect("EIP-1014 GPU search should complete")
        else {
            panic!("EIP-1014 example 0 should match counter zero");
        };
        assert_eq!(
            eip_result.address.to_string(),
            "0x4d1a2e2bb4f88f0250f26ffff098b0b30b26bf38"
        );

        // A full-address target outside the range proves a no-match dispatch.
        let outside = create2_address(deployer, salt_from_counter(10_000), &init_code);
        let no_match_miner = Create2Miner::new(
            deployer,
            &init_code,
            VanityPattern::new(&outside.to_string(), "").unwrap(),
        );
        let mut no_match_session = no_match_miner
            .backend_session(BackendPreference::Gpu)
            .expect("the selected GPU should remain available");
        assert_eq!(
            no_match_miner
                .search_with_backend(
                    &mut no_match_session,
                    MiningOptions {
                        start_counter: 100,
                        max_attempts: Some(67),
                        batch_size: 67,
                    },
                    &AtomicBool::new(false),
                    |_| {},
                )
                .unwrap(),
            SearchOutcome::NotFound {
                candidates_checked: 67
            }
        );

        // Odd prefix and suffix nibbles exercise packed half-byte masks.
        let target_text = target_address.to_string();
        let odd_pattern =
            VanityPattern::new(&target_text[2..3], &target_text[target_text.len() - 1..]).unwrap();
        let odd_miner = Create2Miner::new(deployer, &init_code, odd_pattern.clone());
        let mut odd_session = odd_miner
            .backend_session(BackendPreference::Gpu)
            .expect("the selected GPU should remain available");
        let SearchOutcome::Found(odd_result) = odd_miner
            .search_with_backend(
                &mut odd_session,
                MiningOptions {
                    max_attempts: Some(4096),
                    batch_size: 127,
                    ..MiningOptions::default()
                },
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap()
        else {
            panic!("odd-nibble GPU search should find a match");
        };
        assert!(odd_pattern.matches(&odd_result.address));
        assert_eq!(
            create2_address(deployer, odd_result.salt, &init_code),
            odd_result.address
        );
    }
}
