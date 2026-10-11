// Lucky Lode — Argon2id fill kernel (v1, one work-item per instance).
//
// Mirrors `lm-miner/src/argon2_prep.rs::fill` line for line: same order,
// same arithmetic, same edge cases. The Rust CPU mirror is the executable
// spec; this kernel must reproduce it bit-for-bit (diff gate before any
// mining use — doc 15 §15.4).
//
// Scope: ONLY the memory-hard fill for the frozen EmberHash params
// (Argon2id, p=1, m=1024 KiB, t=3, v=0x13). H0, the initial two blocks,
// and the final H' are computed on the CPU (blake2 crate) and passed in.
//
// Layout: one work-item owns one instance; its 1 MiB block array lives in
// global memory (`mem`), staged in private arrays for compression (v1
// accepts the spilling; v2 will move to __local staging, doc 15 §15.2).
//
// Buffers (all u64 words):
//   init2  : 2 * 128   — initial blocks B[0], B[1]
//   mem    : 1024 * 128 — per instance, 1 MiB
//   out    : 128        — final block B[1023]

#define BLOCK_WORDS 128
#define LANE_LENGTH 1024
#define SEGMENT_LENGTH 256
#define PASSES 3

#define TRUNC_MASK 0xffffffffUL

inline ulong rotr64(ulong x, uint n)
{
    return (x >> n) | (x << (64 - n));
}

// BLAKE2b-style G with the BlaMka multiplication (Argon2's permutation).
inline void blamka_step(ulong *a, ulong *b, ulong *c, ulong *d)
{
    *a = *a + *b + 2UL * ((*a & TRUNC_MASK) * (*b & TRUNC_MASK));
    *d = rotr64(*d ^ *a, 32);
    *c = *c + *d + 2UL * ((*c & TRUNC_MASK) * (*d & TRUNC_MASK));
    *b = rotr64(*b ^ *c, 24);
    *a = *a + *b + 2UL * ((*a & TRUNC_MASK) * (*b & TRUNC_MASK));
    *d = rotr64(*d ^ *a, 16);
    *c = *c + *d + 2UL * ((*c & TRUNC_MASK) * (*d & TRUNC_MASK));
    *b = rotr64(*b ^ *c, 63);
}

inline void permute16(ulong *v)
{
    blamka_step(&v[0], &v[4], &v[8], &v[12]);
    blamka_step(&v[1], &v[5], &v[9], &v[13]);
    blamka_step(&v[2], &v[6], &v[10], &v[14]);
    blamka_step(&v[3], &v[7], &v[11], &v[15]);
    blamka_step(&v[0], &v[5], &v[10], &v[15]);
    blamka_step(&v[1], &v[6], &v[11], &v[12]);
    blamka_step(&v[2], &v[7], &v[8], &v[13]);
    blamka_step(&v[3], &v[4], &v[9], &v[14]);
}

// P(q) ^ r, in place on q (private arrays only; the kernel is
// OpenCL-1.2-safe, so every parameter carries an explicit address space).
inline void permute_and_xor(ulong *q, const ulong *r)
{
    for (int row = 0; row < 8; row++)
        permute16(&q[row * 16]);
    for (int col = 0; col < 8; col++)
    {
        int b = col * 2;
        ulong v[16];
        v[0] = q[b];      v[1] = q[b + 1];
        v[2] = q[b + 16]; v[3] = q[b + 17];
        v[4] = q[b + 32]; v[5] = q[b + 33];
        v[6] = q[b + 48]; v[7] = q[b + 49];
        v[8] = q[b + 64]; v[9] = q[b + 65];
        v[10] = q[b + 80]; v[11] = q[b + 81];
        v[12] = q[b + 96]; v[13] = q[b + 97];
        v[14] = q[b + 112]; v[15] = q[b + 113];
        permute16(v);
        q[b] = v[0];      q[b + 1] = v[1];
        q[b + 16] = v[2]; q[b + 17] = v[3];
        q[b + 32] = v[4]; q[b + 33] = v[5];
        q[b + 48] = v[6]; q[b + 49] = v[7];
        q[b + 64] = v[8]; q[b + 65] = v[9];
        q[b + 80] = v[10]; q[b + 81] = v[11];
        q[b + 96] = v[12]; q[b + 97] = v[13];
        q[b + 112] = v[14]; q[b + 113] = v[15];
    }
    for (int i = 0; i < BLOCK_WORDS; i++)
        q[i] ^= r[i];
}

// Argon2 compression G: P(prev ^ ref) ^ (prev ^ ref), private buffers.
inline void compress_pp(const ulong *prev, const ulong *ref, ulong *out)
{
    ulong r[BLOCK_WORDS];
    for (int i = 0; i < BLOCK_WORDS; i++)
        r[i] = prev[i] ^ ref[i];
    for (int i = 0; i < BLOCK_WORDS; i++)
        out[i] = r[i];
    permute_and_xor(out, r);
}

// Same, reading the two source blocks from global memory (fill path).
inline void compress_gp(__global const ulong *prev, __global const ulong *ref, ulong *out)
{
    ulong r[BLOCK_WORDS];
    for (int i = 0; i < BLOCK_WORDS; i++)
        r[i] = prev[i] ^ ref[i];
    for (int i = 0; i < BLOCK_WORDS; i++)
        out[i] = r[i];
    permute_and_xor(out, r);
}

// addr = G(zero, G(zero, input_with_incremented_counter))  (private path)
inline void next_address_block(ulong *input_block, ulong *address_block, ulong *zero)
{
    input_block[6] += 1;
    ulong first[BLOCK_WORDS];
    compress_pp(zero, input_block, first);
    compress_pp(zero, first, address_block);
}

__kernel void argon2id_fill(__global const ulong *init2,
                            __global ulong *mem,
                            __global ulong *out)
{
    const size_t instance = get_global_id(0);
    __global ulong *B = mem + instance * (size_t)LANE_LENGTH * BLOCK_WORDS;
    __global const ulong *init = init2 + instance * (size_t)2 * BLOCK_WORDS;

    // Initial two blocks (from the CPU's blake2b_long).
    for (int i = 0; i < 2 * BLOCK_WORDS; i++)
        B[i] = init[i];

    ulong zero[BLOCK_WORDS];
    for (int i = 0; i < BLOCK_WORDS; i++)
        zero[i] = 0;

    for (int pass = 0; pass < PASSES; pass++)
    {
        for (int slice = 0; slice < 4; slice++)
        {
            int data_independent = (pass == 0 && slice < 2);
            ulong input_block[BLOCK_WORDS];
            ulong address_block[BLOCK_WORDS];
            if (data_independent)
            {
                for (int i = 0; i < BLOCK_WORDS; i++)
                {
                    input_block[i] = 0;
                    address_block[i] = 0;
                }
                input_block[0] = (ulong)pass;
                input_block[1] = 0; // lane (p=1)
                input_block[2] = (ulong)slice;
                input_block[3] = (ulong)LANE_LENGTH;
                input_block[4] = (ulong)PASSES;
                input_block[5] = 2; // Argon2id
            }

            int first_block = 0;
            if (pass == 0 && slice == 0)
            {
                if (data_independent)
                    next_address_block(input_block, address_block, zero);
                first_block = 2;
            }

            int cur_index = slice * SEGMENT_LENGTH + first_block;
            int prev_index = (slice == 0 && first_block == 0)
                                 ? cur_index + LANE_LENGTH - 1
                                 : cur_index - 1;

            for (int block = first_block; block < SEGMENT_LENGTH; block++)
            {
                ulong rand;
                if (data_independent)
                {
                    int address_index = block % 128;
                    if (address_index == 0)
                        next_address_block(input_block, address_block, zero);
                    rand = address_block[address_index];
                }
                else
                {
                    rand = B[(size_t)prev_index * BLOCK_WORDS];
                }

                // p=1: same lane always.
                int reference_area_size;
                if (pass == 0)
                {
                    if (slice == 0)
                        reference_area_size = block - 1;
                    else
                        reference_area_size = slice * SEGMENT_LENGTH + block - 1;
                }
                else
                {
                    reference_area_size = LANE_LENGTH - SEGMENT_LENGTH + block - 1;
                }

                ulong map = rand & TRUNC_MASK;
                map = (map * map) >> 32;
                int relative_position =
                    reference_area_size - 1 -
                    (int)(((ulong)reference_area_size * map) >> 32);
                int start_position =
                    (pass != 0 && slice != 3) ? (slice + 1) * SEGMENT_LENGTH : 0;
                int ref_index = (start_position + relative_position) % LANE_LENGTH;

                ulong result[BLOCK_WORDS];
                compress_gp(&B[(size_t)prev_index * BLOCK_WORDS],
                            &B[(size_t)ref_index * BLOCK_WORDS],
                            result);
                if (pass == 0)
                {
                    for (int i = 0; i < BLOCK_WORDS; i++)
                        B[(size_t)cur_index * BLOCK_WORDS + i] = result[i];
                }
                else
                {
                    for (int i = 0; i < BLOCK_WORDS; i++)
                        B[(size_t)cur_index * BLOCK_WORDS + i] ^= result[i];
                }
                prev_index = cur_index;
                cur_index += 1;
            }
        }
    }

    // Final block for the CPU's H' finalization.
    for (int i = 0; i < BLOCK_WORDS; i++)
        out[instance * BLOCK_WORDS + i] = B[(size_t)(LANE_LENGTH - 1) * BLOCK_WORDS + i];
}
