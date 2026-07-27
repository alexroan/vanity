use crate::create2::Address;
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Clone, Debug)]
pub struct FoundryProject {
    out_dir: PathBuf,
    create2_deployer: Address,
    contracts: Vec<ContractArtifact>,
}

#[derive(Debug)]
struct FoundryConfig {
    out: PathBuf,
    create2_deployer: Address,
}

impl FoundryProject {
    /// Builds the Foundry project containing `start`, then loads its fresh
    /// creation-bytecode artifacts.
    pub fn build(start: &Path) -> Result<Self> {
        let root = find_project_root(start)?;
        let build = Command::new("forge")
            .arg("build")
            .current_dir(&root)
            .output()
            .with_context(
                || "could not run `forge build`; install Foundry and ensure `forge` is on PATH",
            )?;
        ensure_success("forge build", &build)?;

        let config = Command::new("forge")
            .args(["config", "--json"])
            .current_dir(&root)
            .output()
            .context("could not run `forge config --json`")?;
        ensure_success("forge config --json", &config)?;

        let config = parse_foundry_config(&config.stdout)?;
        let configured_out = config.out;
        let out_dir = if configured_out.is_absolute() {
            configured_out
        } else {
            root.join(configured_out)
        }
        .canonicalize()
        .context("could not resolve Foundry's configured artifact directory")?;

        let contracts = discover_artifacts(&out_dir)?;
        if contracts.is_empty() {
            bail!(
                "forge built successfully, but no deployable creation-bytecode artifacts were found in {}",
                out_dir.display()
            );
        }

        Ok(Self {
            out_dir,
            create2_deployer: config.create2_deployer,
            contracts,
        })
    }

    pub fn out_dir(&self) -> &Path {
        &self.out_dir
    }

    pub const fn create2_deployer(&self) -> Address {
        self.create2_deployer
    }

    pub fn contracts(&self) -> &[ContractArtifact] {
        &self.contracts
    }
}

fn parse_foundry_config(output: &[u8]) -> Result<FoundryConfig> {
    let config: Value =
        serde_json::from_slice(output).context("forge returned invalid JSON config")?;
    let out = config
        .get("out")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("forge config did not contain a string `out` path"))?;
    let create2_deployer = config
        .get("create2_deployer")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("forge config did not contain a string `create2_deployer` address"))?
        .parse::<Address>()
        .map_err(anyhow::Error::msg)
        .context("forge config contained an invalid `create2_deployer` address")?;

    Ok(FoundryConfig {
        out: PathBuf::from(out),
        create2_deployer,
    })
}

#[derive(Clone, Debug)]
pub struct ContractArtifact {
    label: String,
    artifact_path: PathBuf,
    creation_bytecode: String,
    constructor: ConstructorSpec,
    link_references: BTreeMap<LibraryId, Vec<LinkLocation>>,
}

impl ContractArtifact {
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn constructor(&self) -> &ConstructorSpec {
        &self.constructor
    }

    pub fn required_libraries(&self) -> Result<Vec<LibraryId>> {
        let object = bytecode_body(&self.creation_bytecode);
        let mut required = BTreeSet::new();

        for (library, locations) in &self.link_references {
            for location in locations {
                let range = location.char_range(object.len())?;
                if !object.as_bytes()[range].iter().all(u8::is_ascii_hexdigit) {
                    required.insert(library.clone());
                    break;
                }
            }
        }

        Ok(required.into_iter().collect())
    }

    /// Produces the exact CREATE2 init code: linked artifact creation bytecode
    /// followed by ABI-encoded constructor arguments.
    pub fn init_code(
        &self,
        libraries: &BTreeMap<LibraryId, Address>,
        constructor_arguments: &[u8],
    ) -> Result<Vec<u8>> {
        let mut object = bytecode_body(&self.creation_bytecode).to_owned();

        for (library, locations) in &self.link_references {
            for location in locations {
                let range = location.char_range(object.len())?;
                if object.as_bytes()[range.clone()]
                    .iter()
                    .all(u8::is_ascii_hexdigit)
                {
                    continue;
                }

                let address = libraries.get(library).ok_or_else(|| {
                    anyhow!("artifact still needs a deployed address for library {library}")
                })?;
                if location.length != 20 {
                    bail!(
                        "library reference {library} has unsupported length {} bytes (expected 20)",
                        location.length
                    );
                }
                object.replace_range(range, &hex::encode(address.as_bytes()));
            }
        }

        let mut init_code = hex::decode(&object).with_context(|| {
            format!(
                "{} creation bytecode is not valid hex; it may contain an unresolved library placeholder",
                self.label
            )
        })?;
        init_code.extend_from_slice(constructor_arguments);
        Ok(init_code)
    }
}

impl fmt::Display for ContractArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.label)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConstructorSpec {
    input_types: Vec<String>,
}

impl ConstructorSpec {
    pub fn requires_arguments(&self) -> bool {
        !self.input_types.is_empty()
    }

    pub fn signature(&self) -> String {
        format!("constructor({})", self.input_types.join(","))
    }

    /// `cast abi-encode` accepts a function-shaped signature. Constructor
    /// arguments use the same ABI tuple encoding, so this is convenient to
    /// copy into a shell.
    pub fn cast_signature(&self) -> String {
        format!("args({})", self.input_types.join(","))
    }

    pub fn validate_arguments(&self, encoded: &[u8]) -> Result<()> {
        let parameter_types = self
            .input_types
            .iter()
            .map(|input_type| {
                // Solidity ABI encodes an external function pointer exactly
                // like bytes24 (20-byte address + 4-byte selector).
                let validation_type = input_type.replace("function", "bytes24");
                let parsed = ethabi::param_type::Reader::read(&validation_type)
                    .with_context(|| format!("unsupported constructor ABI type `{input_type}`"))?;
                if ethabi::param_type::Writer::write(&parsed) != validation_type
                    || !is_supported_abi_type(&parsed)
                {
                    bail!("unsupported constructor ABI type `{input_type}`");
                }
                Ok(parsed)
            })
            .collect::<Result<Vec<_>>>()?;
        let tokens = ethabi::decode(&parameter_types, encoded)
            .context("bytes are not valid for this constructor signature")?;
        if !parameter_types
            .iter()
            .zip(&tokens)
            .all(|(parameter_type, token)| token_fits_type(parameter_type, token))
        {
            bail!("constructor arguments contain a value outside its ABI type");
        }
        let canonical = ethabi::encode(&tokens);
        if canonical != encoded {
            bail!("constructor arguments are not canonically ABI encoded");
        }
        Ok(())
    }
}

fn is_supported_abi_type(parameter_type: &ethabi::ParamType) -> bool {
    match parameter_type {
        ethabi::ParamType::Int(bits) | ethabi::ParamType::Uint(bits) => {
            (8..=256).contains(bits) && bits % 8 == 0
        }
        ethabi::ParamType::FixedBytes(length) => (1..=32).contains(length),
        ethabi::ParamType::Array(element) | ethabi::ParamType::FixedArray(element, _) => {
            is_supported_abi_type(element)
        }
        ethabi::ParamType::Tuple(elements) => elements.iter().all(is_supported_abi_type),
        _ => true,
    }
}

fn token_fits_type(parameter_type: &ethabi::ParamType, token: &ethabi::Token) -> bool {
    match (parameter_type, token) {
        (ethabi::ParamType::Uint(bits), ethabi::Token::Uint(value)) => {
            *bits == 256 || value.bits() <= *bits
        }
        (ethabi::ParamType::Int(bits), ethabi::Token::Int(value)) => {
            if *bits == 256 {
                return true;
            }
            let sign = value.bit(*bits - 1);
            (*bits..256).all(|bit| value.bit(bit) == sign)
        }
        (ethabi::ParamType::Array(element), ethabi::Token::Array(values))
        | (ethabi::ParamType::FixedArray(element, _), ethabi::Token::FixedArray(values)) => {
            values.iter().all(|value| token_fits_type(element, value))
        }
        (ethabi::ParamType::Tuple(types), ethabi::Token::Tuple(values)) => {
            types.len() == values.len()
                && types
                    .iter()
                    .zip(values)
                    .all(|(element_type, value)| token_fits_type(element_type, value))
        }
        // The decoder already guarantees token shape for all other types.
        _ => true,
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LibraryId {
    pub source: String,
    pub name: String,
}

impl fmt::Display for LibraryId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.source, self.name)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct RawArtifact {
    #[serde(default)]
    abi: Vec<RawAbiItem>,
    bytecode: RawBytecode,
    #[serde(default)]
    metadata: Value,
    #[serde(default, rename = "rawMetadata")]
    raw_metadata: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct RawBytecode {
    object: String,
    #[serde(default, rename = "linkReferences")]
    link_references: BTreeMap<String, BTreeMap<String, Vec<LinkLocation>>>,
}

#[derive(Clone, Debug, Deserialize)]
struct RawAbiItem {
    #[serde(rename = "type")]
    item_type: String,
    #[serde(default)]
    inputs: Vec<RawAbiInput>,
}

#[derive(Clone, Debug, Deserialize)]
struct RawAbiInput {
    #[serde(rename = "type")]
    input_type: String,
    #[serde(default)]
    components: Vec<RawAbiInput>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
struct LinkLocation {
    start: usize,
    length: usize,
}

impl LinkLocation {
    fn char_range(self, object_len: usize) -> Result<std::ops::Range<usize>> {
        let start = self
            .start
            .checked_mul(2)
            .ok_or_else(|| anyhow!("library link offset overflowed"))?;
        let length = self
            .length
            .checked_mul(2)
            .ok_or_else(|| anyhow!("library link length overflowed"))?;
        let end = start
            .checked_add(length)
            .ok_or_else(|| anyhow!("library link range overflowed"))?;
        if end > object_len {
            bail!("library link range {start}..{end} exceeds bytecode length {object_len}");
        }
        Ok(start..end)
    }
}

pub fn find_project_root(start: &Path) -> Result<PathBuf> {
    let start = start
        .canonicalize()
        .with_context(|| format!("could not access {}", start.display()))?;
    let start = if start.is_file() {
        start
            .parent()
            .ok_or_else(|| anyhow!("{} has no parent directory", start.display()))?
            .to_owned()
    } else {
        start
    };

    for ancestor in start.ancestors() {
        if ancestor.join("foundry.toml").is_file() {
            return Ok(ancestor.to_owned());
        }
    }

    // A zero-config Foundry project uses the default `src` directory. Search
    // ancestors so invocation from `src/` or any deeper directory still works.
    for ancestor in start.ancestors() {
        let source_dir = ancestor.join("src");
        if source_dir.is_dir() && contains_solidity_source(&source_dir) {
            return Ok(ancestor.to_owned());
        }
    }

    bail!(
        "could not find a Foundry project from {}; run `vanity` inside a project containing foundry.toml or Solidity sources under src/",
        start.display()
    )
}

fn contains_solidity_source(directory: &Path) -> bool {
    let Ok(entries) = fs::read_dir(directory) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() && contains_solidity_source(&path) {
            return true;
        }
        if file_type.is_file() && path.extension() == Some(OsStr::new("sol")) {
            return true;
        }
    }
    false
}

pub fn discover_artifacts(out_dir: &Path) -> Result<Vec<ContractArtifact>> {
    if !out_dir.is_dir() {
        bail!(
            "Foundry artifact directory does not exist: {}",
            out_dir.display()
        );
    }

    let mut json_paths = Vec::new();
    collect_json_paths(out_dir, &mut json_paths)?;
    json_paths.sort();

    let mut contracts = Vec::new();
    for path in json_paths {
        let bytes = fs::read(&path)
            .with_context(|| format!("could not read artifact {}", path.display()))?;
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid JSON in {}", path.display()))?;

        // build-info and other Foundry JSON files are not contract artifacts.
        if value
            .pointer("/bytecode/object")
            .and_then(Value::as_str)
            .is_none()
        {
            continue;
        }

        let raw: RawArtifact = serde_json::from_value(value.clone())
            .with_context(|| format!("invalid contract artifact {}", path.display()))?;
        if bytecode_body(&raw.bytecode.object).is_empty() {
            continue;
        }

        let (source, contract) = artifact_identity(&value, &raw, &path);
        let label = format!("{source}:{contract}");
        let constructor = raw
            .abi
            .iter()
            .find(|item| item.item_type == "constructor")
            .map(|item| ConstructorSpec {
                input_types: item.inputs.iter().map(canonical_abi_type).collect(),
            })
            .unwrap_or_default();

        let mut link_references = BTreeMap::new();
        for (library_source, libraries) in raw.bytecode.link_references {
            for (library_name, locations) in libraries {
                link_references.insert(
                    LibraryId {
                        source: library_source.clone(),
                        name: library_name,
                    },
                    locations,
                );
            }
        }

        contracts.push(ContractArtifact {
            label,
            artifact_path: path,
            creation_bytecode: raw.bytecode.object,
            constructor,
            link_references,
        });
    }

    contracts.sort_by(|left, right| {
        left.label
            .cmp(&right.label)
            .then_with(|| left.artifact_path.cmp(&right.artifact_path))
    });
    Ok(contracts)
}

fn artifact_identity(value: &Value, raw: &RawArtifact, path: &Path) -> (String, String) {
    if let Some((source, contract)) = compilation_target(&raw.metadata) {
        return (source, contract);
    }

    if let Some(raw_metadata) = &raw.raw_metadata {
        if let Ok(metadata) = serde_json::from_str::<Value>(raw_metadata) {
            if let Some(identity) = compilation_target(&metadata) {
                return identity;
            }
        }
    }

    if let Some((source, contract)) = value
        .pointer("/metadata/settings/compilationTarget")
        .and_then(Value::as_object)
        .and_then(|targets| targets.iter().next())
    {
        return (
            source.clone(),
            contract.as_str().unwrap_or("unknown").to_owned(),
        );
    }

    let contract = path
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("unknown")
        .to_owned();
    let source = path
        .parent()
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .unwrap_or("unknown")
        .to_owned();
    (source, contract)
}

fn compilation_target(metadata: &Value) -> Option<(String, String)> {
    metadata
        .pointer("/settings/compilationTarget")
        .and_then(Value::as_object)
        .and_then(|targets| targets.iter().next())
        .map(|(source, contract)| {
            (
                source.clone(),
                contract.as_str().unwrap_or("unknown").to_owned(),
            )
        })
}

fn canonical_abi_type(input: &RawAbiInput) -> String {
    let Some(tuple_suffix) = input.input_type.strip_prefix("tuple") else {
        return input.input_type.clone();
    };
    let components = input
        .components
        .iter()
        .map(canonical_abi_type)
        .collect::<Vec<_>>()
        .join(",");
    format!("({components}){tuple_suffix}")
}

fn collect_json_paths(directory: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("could not read directory {}", directory.display()))?
    {
        let entry = entry
            .with_context(|| format!("could not read an entry beneath {}", directory.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("could not inspect {}", path.display()))?;
        if file_type.is_dir() {
            collect_json_paths(&path, paths)?;
        } else if file_type.is_file() && path.extension() == Some(OsStr::new("json")) {
            paths.push(path);
        }
    }
    Ok(())
}

fn bytecode_body(object: &str) -> &str {
    object
        .strip_prefix("0x")
        .or_else(|| object.strip_prefix("0X"))
        .unwrap_or(object)
}

fn ensure_success(command: &str, output: &Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut details = Vec::new();
    if !stdout.trim().is_empty() {
        details.push(format!("stdout:\n{}", stdout.trim()));
    }
    if !stderr.trim().is_empty() {
        details.push(format!("stderr:\n{}", stderr.trim()));
    }
    let details = details.join("\n");
    bail!(
        "`{command}` failed with status {}{}",
        output.status,
        if details.is_empty() {
            String::new()
        } else {
            format!(":\n{details}")
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_json(path: &Path, value: &Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    }

    fn artifact(source: &str, contract: &str, bytecode: &str, abi: Value, links: Value) -> Value {
        json!({
            "abi": abi,
            "bytecode": {
                "object": bytecode,
                "linkReferences": links
            },
            "metadata": {
                "settings": {
                    "compilationTarget": {
                        source: contract
                    }
                }
            }
        })
    }

    #[test]
    fn discovers_deployable_artifacts_and_skips_other_json() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("custom-out");
        write_json(
            &out.join("Foo.sol/Foo.json"),
            &artifact("src/Foo.sol", "Foo", "0x6000", json!([]), json!({})),
        );
        write_json(
            &out.join("IFoo.sol/IFoo.json"),
            &artifact("src/IFoo.sol", "IFoo", "0x", json!([]), json!({})),
        );
        write_json(
            &out.join("build-info/id.json"),
            &json!({"id": "abc", "language": "Solidity"}),
        );

        let contracts = discover_artifacts(&out).unwrap();
        assert_eq!(contracts.len(), 1);
        assert_eq!(contracts[0].label(), "src/Foo.sol:Foo");
        assert_eq!(
            contracts[0].init_code(&BTreeMap::new(), &[]).unwrap(),
            [0x60, 0x00]
        );
    }

    #[test]
    fn records_constructor_types_including_tuples() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out");
        write_json(
            &out.join("WithArgs.sol/WithArgs.json"),
            &artifact(
                "src/WithArgs.sol",
                "WithArgs",
                "0x6000",
                json!([{
                    "type": "constructor",
                    "inputs": [
                        {"type": "address"},
                        {
                            "type": "tuple[]",
                            "components": [
                                {"type": "uint256"},
                                {"type": "bytes32"}
                            ]
                        }
                    ]
                }]),
                json!({}),
            ),
        );

        let contracts = discover_artifacts(&out).unwrap();
        assert_eq!(
            contracts[0].constructor().signature(),
            "constructor(address,(uint256,bytes32)[])"
        );
        assert_eq!(
            contracts[0].constructor().cast_signature(),
            "args(address,(uint256,bytes32)[])"
        );
    }

    #[test]
    fn constructor_arguments_must_match_the_artifact_abi() {
        let static_constructor = ConstructorSpec {
            input_types: vec!["uint256".to_owned()],
        };
        let valid_static = ethabi::encode(&[ethabi::Token::Uint(42_u64.into())]);
        assert!(static_constructor.validate_arguments(&valid_static).is_ok());
        assert!(static_constructor.validate_arguments(&[0x00]).is_err());

        let dynamic_constructor = ConstructorSpec {
            input_types: vec!["string".to_owned()],
        };
        let valid_dynamic = ethabi::encode(&[ethabi::Token::String("vanity".to_owned())]);
        assert!(
            dynamic_constructor
                .validate_arguments(&valid_dynamic)
                .is_ok()
        );

        let mut trailing_garbage = valid_dynamic;
        trailing_garbage.extend_from_slice(&[0; 32]);
        assert!(
            dynamic_constructor
                .validate_arguments(&trailing_garbage)
                .is_err()
        );

        let narrow_unsigned = ConstructorSpec {
            input_types: vec!["uint8".to_owned()],
        };
        let out_of_range = ethabi::encode(&[ethabi::Token::Uint(ethabi::Uint::from(256))]);
        assert!(narrow_unsigned.validate_arguments(&out_of_range).is_err());

        let narrow_signed = ConstructorSpec {
            input_types: vec!["int8".to_owned()],
        };
        let negative_one = ethabi::encode(&[ethabi::Token::Int(ethabi::Uint::max_value())]);
        assert!(narrow_signed.validate_arguments(&negative_one).is_ok());
        let invalid_positive = ethabi::encode(&[ethabi::Token::Int(ethabi::Uint::from(128))]);
        assert!(narrow_signed.validate_arguments(&invalid_positive).is_err());

        let invalid_type = ConstructorSpec {
            input_types: vec!["uint7".to_owned()],
        };
        assert!(invalid_type.validate_arguments(&[0; 32]).is_err());
    }

    #[test]
    fn constructor_function_pointer_uses_bytes24_abi_encoding() {
        let constructor = ConstructorSpec {
            input_types: vec!["function".to_owned()],
        };
        let encoded = ethabi::encode(&[ethabi::Token::FixedBytes(vec![0x11; 24])]);
        assert!(constructor.validate_arguments(&encoded).is_ok());
    }

    #[test]
    fn links_library_placeholders_before_appending_constructor_arguments() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out");
        let placeholder = "__$5dd90c61c8927ece6bd23496dbbf4e2e87$__";
        write_json(
            &out.join("UsesLib.sol/UsesLib.json"),
            &artifact(
                "src/UsesLib.sol",
                "UsesLib",
                &format!("0x60{placeholder}00"),
                json!([]),
                json!({
                    "src/ExternalMath.sol": {
                        "ExternalMath": [{"start": 1, "length": 20}]
                    }
                }),
            ),
        );

        let contract = discover_artifacts(&out).unwrap().remove(0);
        let library = contract.required_libraries().unwrap().remove(0);
        let address: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let libraries = BTreeMap::from([(library, address)]);
        let init_code = contract.init_code(&libraries, &[0xaa]).unwrap();

        assert_eq!(init_code[0], 0x60);
        assert_eq!(&init_code[1..21], address.as_bytes());
        assert_eq!(&init_code[21..], &[0x00, 0xaa]);
    }

    #[test]
    fn malformed_json_is_reported_with_its_path() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out/Foo.sol");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("Foo.json"), b"{").unwrap();
        let error = discover_artifacts(&temp.path().join("out")).unwrap_err();
        assert!(error.to_string().contains("Foo.json"));
    }

    #[test]
    fn closest_foundry_toml_defines_project_root() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("foundry.toml"), "[profile.default]\n").unwrap();
        let nested = temp.path().join("src/deep");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            find_project_root(&nested).unwrap(),
            temp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn nested_zero_config_project_uses_ancestor_with_solidity_sources() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("src/contracts");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("Counter.sol"), "contract Counter {}").unwrap();
        let nested = source.join("nested");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            find_project_root(&nested).unwrap(),
            temp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn parses_foundry_output_and_create2_deployer() {
        let config = parse_foundry_config(
            br#"{
                "out": "custom-out",
                "create2_deployer": "0x1111111111111111111111111111111111111111"
            }"#,
        )
        .unwrap();

        assert_eq!(config.out, PathBuf::from("custom-out"));
        assert_eq!(
            config.create2_deployer.to_string(),
            "0x1111111111111111111111111111111111111111"
        );
    }

    #[test]
    fn rejects_missing_or_invalid_create2_deployer_configuration() {
        let missing = parse_foundry_config(br#"{"out":"out"}"#).unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("forge config did not contain a string `create2_deployer`")
        );

        let invalid = parse_foundry_config(br#"{"out":"out","create2_deployer":"not-an-address"}"#)
            .unwrap_err();
        assert!(
            invalid
                .to_string()
                .contains("forge config contained an invalid `create2_deployer` address")
        );
    }
}
