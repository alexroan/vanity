struct Params {
    start_counter: u64,
    count: u32,
    tile_start: u32,
    lanes: array<u64, 17>,
    mask: array<u32, 5>,
    value: array<u32, 5>,
}

struct ScanResult {
    match_offset: atomic<u32>,
    witness: array<u32, 8>,
}

@group(0) @binding(0)
var<storage, read> params: Params;

@group(0) @binding(1)
var<storage, read_write> result: ScanResult;

const ROUND_CONSTANTS: array<u64, 24> = array<u64, 24>(
    0x0000000000000001lu, 0x0000000000008082lu,
    0x800000000000808alu, 0x8000000080008000lu,
    0x000000000000808blu, 0x0000000080000001lu,
    0x8000000080008081lu, 0x8000000000008009lu,
    0x000000000000008alu, 0x0000000000000088lu,
    0x0000000080008009lu, 0x000000008000000alu,
    0x000000008000808blu, 0x800000000000008blu,
    0x8000000000008089lu, 0x8000000000008003lu,
    0x8000000000008002lu, 0x8000000000000080lu,
    0x000000000000800alu, 0x800000008000000alu,
    0x8000000080008081lu, 0x8000000000008080lu,
    0x0000000080000001lu, 0x8000000080008008lu,
);

fn rotate_left(value: u64, shift: u32) -> u64 {
    return (value << shift) | (value >> (64u - shift));
}

fn byte_swap(value: u64) -> u64 {
    return ((value & 0x00000000000000fflu) << 56u)
        | ((value & 0x000000000000ff00lu) << 40u)
        | ((value & 0x0000000000ff0000lu) << 24u)
        | ((value & 0x00000000ff000000lu) << 8u)
        | ((value & 0x000000ff00000000lu) >> 8u)
        | ((value & 0x0000ff0000000000lu) >> 24u)
        | ((value & 0x00ff000000000000lu) >> 40u)
        | ((value & 0xff00000000000000lu) >> 56u);
}

fn keccak_round(state: ptr<function, array<u64, 25>>, round_constant: u64) {
    let c0 = (*state)[0u] ^ (*state)[5u] ^ (*state)[10u] ^ (*state)[15u] ^ (*state)[20u];
    let c1 = (*state)[1u] ^ (*state)[6u] ^ (*state)[11u] ^ (*state)[16u] ^ (*state)[21u];
    let c2 = (*state)[2u] ^ (*state)[7u] ^ (*state)[12u] ^ (*state)[17u] ^ (*state)[22u];
    let c3 = (*state)[3u] ^ (*state)[8u] ^ (*state)[13u] ^ (*state)[18u] ^ (*state)[23u];
    let c4 = (*state)[4u] ^ (*state)[9u] ^ (*state)[14u] ^ (*state)[19u] ^ (*state)[24u];
    let d0 = c4 ^ rotate_left(c1, 1u);
    let d1 = c0 ^ rotate_left(c2, 1u);
    let d2 = c1 ^ rotate_left(c3, 1u);
    let d3 = c2 ^ rotate_left(c4, 1u);
    let d4 = c3 ^ rotate_left(c0, 1u);

    (*state)[0u] ^= d0;
    (*state)[5u] ^= d0;
    (*state)[10u] ^= d0;
    (*state)[15u] ^= d0;
    (*state)[20u] ^= d0;
    (*state)[1u] ^= d1;
    (*state)[6u] ^= d1;
    (*state)[11u] ^= d1;
    (*state)[16u] ^= d1;
    (*state)[21u] ^= d1;
    (*state)[2u] ^= d2;
    (*state)[7u] ^= d2;
    (*state)[12u] ^= d2;
    (*state)[17u] ^= d2;
    (*state)[22u] ^= d2;
    (*state)[3u] ^= d3;
    (*state)[8u] ^= d3;
    (*state)[13u] ^= d3;
    (*state)[18u] ^= d3;
    (*state)[23u] ^= d3;
    (*state)[4u] ^= d4;
    (*state)[9u] ^= d4;
    (*state)[14u] ^= d4;
    (*state)[19u] ^= d4;
    (*state)[24u] ^= d4;

    let b0 = (*state)[0u];
    let b10 = rotate_left((*state)[1u], 1u);
    let b20 = rotate_left((*state)[2u], 62u);
    let b5 = rotate_left((*state)[3u], 28u);
    let b15 = rotate_left((*state)[4u], 27u);
    let b16 = rotate_left((*state)[5u], 36u);
    let b1 = rotate_left((*state)[6u], 44u);
    let b11 = rotate_left((*state)[7u], 6u);
    let b21 = rotate_left((*state)[8u], 55u);
    let b6 = rotate_left((*state)[9u], 20u);
    let b7 = rotate_left((*state)[10u], 3u);
    let b17 = rotate_left((*state)[11u], 10u);
    let b2 = rotate_left((*state)[12u], 43u);
    let b12 = rotate_left((*state)[13u], 25u);
    let b22 = rotate_left((*state)[14u], 39u);
    let b23 = rotate_left((*state)[15u], 41u);
    let b8 = rotate_left((*state)[16u], 45u);
    let b18 = rotate_left((*state)[17u], 15u);
    let b3 = rotate_left((*state)[18u], 21u);
    let b13 = rotate_left((*state)[19u], 8u);
    let b14 = rotate_left((*state)[20u], 18u);
    let b24 = rotate_left((*state)[21u], 2u);
    let b9 = rotate_left((*state)[22u], 61u);
    let b19 = rotate_left((*state)[23u], 56u);
    let b4 = rotate_left((*state)[24u], 14u);

    (*state)[0u] = b0 ^ ((~b1) & b2);
    (*state)[1u] = b1 ^ ((~b2) & b3);
    (*state)[2u] = b2 ^ ((~b3) & b4);
    (*state)[3u] = b3 ^ ((~b4) & b0);
    (*state)[4u] = b4 ^ ((~b0) & b1);
    (*state)[5u] = b5 ^ ((~b6) & b7);
    (*state)[6u] = b6 ^ ((~b7) & b8);
    (*state)[7u] = b7 ^ ((~b8) & b9);
    (*state)[8u] = b8 ^ ((~b9) & b5);
    (*state)[9u] = b9 ^ ((~b5) & b6);
    (*state)[10u] = b10 ^ ((~b11) & b12);
    (*state)[11u] = b11 ^ ((~b12) & b13);
    (*state)[12u] = b12 ^ ((~b13) & b14);
    (*state)[13u] = b13 ^ ((~b14) & b10);
    (*state)[14u] = b14 ^ ((~b10) & b11);
    (*state)[15u] = b15 ^ ((~b16) & b17);
    (*state)[16u] = b16 ^ ((~b17) & b18);
    (*state)[17u] = b17 ^ ((~b18) & b19);
    (*state)[18u] = b18 ^ ((~b19) & b15);
    (*state)[19u] = b19 ^ ((~b15) & b16);
    (*state)[20u] = b20 ^ ((~b21) & b22);
    (*state)[21u] = b21 ^ ((~b22) & b23);
    (*state)[22u] = b22 ^ ((~b23) & b24);
    (*state)[23u] = b23 ^ ((~b24) & b20);
    (*state)[24u] = b24 ^ ((~b20) & b21);
    (*state)[0u] ^= round_constant;
}

fn keccak_f(state: ptr<function, array<u64, 25>>) {
    for (var round = 0u; round < 24u; round++) {
        keccak_round(state, ROUND_CONSTANTS[round]);
    }
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) global_id: vec3<u32>) {
    if (global_id.x >= params.count - params.tile_start) {
        return;
    }
    let offset = params.tile_start + global_id.x;

    var state = array<u64, 25>();
    state[0u] = params.lanes[0u];
    state[1u] = params.lanes[1u];
    state[2u] = params.lanes[2u];
    state[3u] = params.lanes[3u];
    state[4u] = params.lanes[4u];
    state[5u] = params.lanes[5u];
    state[6u] = params.lanes[6u];
    state[7u] = params.lanes[7u];
    state[8u] = params.lanes[8u];
    state[9u] = params.lanes[9u];
    state[10u] = params.lanes[10u];
    state[11u] = params.lanes[11u];
    state[12u] = params.lanes[12u];
    state[13u] = params.lanes[13u];
    state[14u] = params.lanes[14u];
    state[15u] = params.lanes[15u];
    state[16u] = params.lanes[16u];
    let counter = params.start_counter + u64(offset);
    let swapped = byte_swap(counter);
    state[5u] |= swapped << 40u;
    state[6u] |= swapped >> 24u;
    keccak_f(&state);

    if (offset == 0u) {
        result.witness[0u] = u32(state[0u]);
        result.witness[1u] = u32(state[0u] >> 32u);
        result.witness[2u] = u32(state[1u]);
        result.witness[3u] = u32(state[1u] >> 32u);
        result.witness[4u] = u32(state[2u]);
        result.witness[5u] = u32(state[2u] >> 32u);
        result.witness[6u] = u32(state[3u]);
        result.witness[7u] = u32(state[3u] >> 32u);
    }

    if ((u32(state[1u] >> 32u) & params.mask[0u]) == params.value[0u]
        && (u32(state[2u]) & params.mask[1u]) == params.value[1u]
        && (u32(state[2u] >> 32u) & params.mask[2u]) == params.value[2u]
        && (u32(state[3u]) & params.mask[3u]) == params.value[3u]
        && (u32(state[3u] >> 32u) & params.mask[4u]) == params.value[4u]) {
        atomicMin(&result.match_offset, offset);
    }
}
