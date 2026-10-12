// Lucky Lode — Argon2id fill kernel (v2, workgroup-per-instance).
//
// Mirrors `lm-miner/src/argon2_prep.rs::fill` operation for operation.
// The CPU mirror is the executable spec; this kernel must reproduce it
// bit-for-bit (KAT + 1024-case battery gate before any mining use — doc
// 15 §15.4).
//
// v1 (one work-item per instance) measured ~2.4-2.8 kH/s on the GTX
// 1080: 255 registers + 3.4 KiB stack of spills per work-item, and —
// worse — each warp's lanes touched 32 different 1 MiB-apart blocks, so
// every global access wasted 7/8 of a 32 B sector. v2 fixes both:
//
//   * one WORKGROUP of 32 items handles 4 instances (8 items each);
//   * the 8 items of an instance split the 128-word block (one row of
//     16 words per item), so a warp instruction spanning an instance
//     touches consecutive words of ONE block — coalesced;
//   * the row-permuted block stages in __local (1 KiB per instance),
//     exchanged with two barriers per compression; a third barrier
//     separates write-back from the next row staging;
//   * per-item registers are a 16-word vector + a 16-word row (~90
//     regs), so shared capacity (4 KiB/group), not registers, caps
//     occupancy at 12 warps/SM.
//
// Per compression: r = prev ^ ref (coalesced ulong4), row permute (in
// registers), stage to __local, column permute (from __local), write
// back result ^ r (coalesced xor for passes > 0). Address generation
// (pass 0, slices 0-1) reuses the same machinery with prev = zero and
// per-instance input/mid/address scratch in global memory.
//
// Buffers (all u64 words):
//   init2  : 2 * 128          — initial blocks B[0], B[1]
//   mem    : 1024 * 128       — per instance, 1 MiB
//   out    : 128              — final block B[1023]
//   scratch: 3 * 128          — per instance: input | mid | address
//
// Batches are padded to a multiple of 4 instances by the host; every
// workgroup is fully populated (no inactive instances inside barriers).

#define BLOCK_WORDS 128
#define LANE_LENGTH 1024
#define SEGMENT_LENGTH 256
#define PASSES 3
#define SCRATCH_WORDS 384
#define ITEMS 8
#define INSTANCES_PER_GROUP 4
#define GROUP_ITEMS (ITEMS * INSTANCES_PER_GROUP)
// Per-instance shared stride: 128 words + 4 of padding so the four
// instances' blocks land on different shared banks (128 % 32 == 0 would
// make all four clash on every gather).
#define Q_STRIDE 132

#define TRUNC_MASK 0xffffffffUL

#define WG_BARRIER() barrier(CLK_LOCAL_MEM_FENCE | CLK_GLOBAL_MEM_FENCE)

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

// One compression, cooperatively by the 8 items of an instance.
// dst = P(prev ^ ref) ^ prev ^ ref (xormode: dst ^= result).
// prev4 == 0 means the zero block (address path).
// `q` is this instance's 128-word __local staging block.
inline void compress_inst(__global const ulong4 *prev4,
                          __global const ulong4 *ref4,
                          __global ulong4 *dst4,
                          __local ulong *q,
                          int item,
                          int xormode)
{
    // Row transform: item owns row `item` (16 words) of r = prev ^ ref.
    ulong v[16];
    ulong4 r4[4];
    #pragma unroll
    for (int k = 0; k < 4; k++)
    {
        ulong4 f = ref4[item * 4 + k];
        if (prev4 != 0)
        {
            ulong4 p = prev4[item * 4 + k];
            f.x ^= p.x;
            f.y ^= p.y;
            f.z ^= p.z;
            f.w ^= p.w;
        }
        r4[k] = f;
        v[4 * k] = f.x;
        v[4 * k + 1] = f.y;
        v[4 * k + 2] = f.z;
        v[4 * k + 3] = f.w;
    }
    permute16(v);
    #pragma unroll
    for (int k = 0; k < 4; k++)
    {
        q[item * 16 + 4 * k] = v[4 * k];
        q[item * 16 + 4 * k + 1] = v[4 * k + 1];
        q[item * 16 + 4 * k + 2] = v[4 * k + 2];
        q[item * 16 + 4 * k + 3] = v[4 * k + 3];
    }
    WG_BARRIER();

    // Column transform: item owns column pair (2*item, 2*item+1).
    int b = 2 * item;
    v[0] = q[b];        v[1] = q[b + 1];
    v[2] = q[b + 16];   v[3] = q[b + 17];
    v[4] = q[b + 32];   v[5] = q[b + 33];
    v[6] = q[b + 48];   v[7] = q[b + 49];
    v[8] = q[b + 64];   v[9] = q[b + 65];
    v[10] = q[b + 80];  v[11] = q[b + 81];
    v[12] = q[b + 96];  v[13] = q[b + 97];
    v[14] = q[b + 112]; v[15] = q[b + 113];
    permute16(v);
    q[b] = v[0];        q[b + 1] = v[1];
    q[b + 16] = v[2];   q[b + 17] = v[3];
    q[b + 32] = v[4];   q[b + 33] = v[5];
    q[b + 48] = v[6];   q[b + 49] = v[7];
    q[b + 64] = v[8];   q[b + 65] = v[9];
    q[b + 80] = v[10];  q[b + 81] = v[11];
    q[b + 96] = v[12];  q[b + 97] = v[13];
    q[b + 112] = v[14]; q[b + 113] = v[15];
    WG_BARRIER();

    // Write back: row = staged row ^ r; coalesced global stores.
    #pragma unroll
    for (int k = 0; k < 4; k++)
    {
        ulong4 res;
        res.x = q[item * 16 + 4 * k] ^ r4[k].x;
        res.y = q[item * 16 + 4 * k + 1] ^ r4[k].y;
        res.z = q[item * 16 + 4 * k + 2] ^ r4[k].z;
        res.w = q[item * 16 + 4 * k + 3] ^ r4[k].w;
        if (xormode)
        {
            ulong4 o = dst4[item * 4 + k];
            o.x ^= res.x;
            o.y ^= res.y;
            o.z ^= res.z;
            o.w ^= res.w;
            dst4[item * 4 + k] = o;
        }
        else
        {
            dst4[item * 4 + k] = res;
        }
    }
    // No barrier after write-back: it reads ONLY this item's row, and the
    // next compression's row phase writes ONLY this item's row — same
    // item, program order. Cross-item safety comes from the first barrier
    // of the next compression (every item's write-back precedes its row
    // staging, so all reads are done before any column gather).
}

// addr = G(zero, G(zero, input_with_incremented_counter)), cooperatively.
inline void next_address_block(__global ulong *input,
                               __global ulong *mid,
                               __global ulong *address,
                               __local ulong *q,
                               int item)
{
    if (item == 0)
        input[6] += 1;
    WG_BARRIER();
    compress_inst((__global const ulong4 *)0, (__global const ulong4 *)input,
                  (__global ulong4 *)mid, q, item, 0);
    compress_inst((__global const ulong4 *)0, (__global const ulong4 *)mid,
                  (__global ulong4 *)address, q, item, 0);
}

__kernel void argon2id_fill(__global const ulong *init2,
                            __global ulong *mem,
                            __global ulong *out,
                            __global ulong *scratch)
{
    const int lid = get_local_id(0);
    const int inst_in_group = lid / ITEMS;
    const int item = lid % ITEMS;
    const size_t instance = (size_t)get_group_id(0) * INSTANCES_PER_GROUP + inst_in_group;

    __local ulong q[INSTANCES_PER_GROUP * Q_STRIDE];
    __local ulong *qi = q + inst_in_group * Q_STRIDE;

    __global ulong *B = mem + instance * (size_t)LANE_LENGTH * BLOCK_WORDS;
    __global ulong *input = scratch + instance * (size_t)SCRATCH_WORDS;
    __global ulong *mid = input + BLOCK_WORDS;
    __global ulong *address = mid + BLOCK_WORDS;

    // Initial two blocks (from the CPU's blake2b_long).
    {
        __global const ulong4 *init4 =
            (__global const ulong4 *)(init2 + instance * (size_t)2 * BLOCK_WORDS);
        __global ulong4 *B4 = (__global ulong4 *)B;
        for (int k = item; k < 2 * BLOCK_WORDS / 4; k += ITEMS)
            B4[k] = init4[k];
    }
    WG_BARRIER();

    for (int pass = 0; pass < PASSES; pass++)
    {
        for (int slice = 0; slice < 4; slice++)
        {
            int data_independent = (pass == 0 && slice < 2);
            if (data_independent)
            {
                for (int i = item; i < BLOCK_WORDS; i += ITEMS)
                    input[i] = 0;
                if (item == 0)
                {
                    input[0] = (ulong)pass;
                    input[1] = 0; // lane (p=1)
                    input[2] = (ulong)slice;
                    input[3] = (ulong)LANE_LENGTH;
                    input[4] = (ulong)PASSES;
                    input[5] = 2; // Argon2id
                }
                WG_BARRIER();
            }

            int first_block = 0;
            if (pass == 0 && slice == 0)
            {
                if (data_independent)
                    next_address_block(input, mid, address, qi, item);
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
                        next_address_block(input, mid, address, qi, item);
                    rand = address[address_index];
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

                __global const ulong4 *prev4 =
                    (__global const ulong4 *)(B + (size_t)prev_index * BLOCK_WORDS);
                __global const ulong4 *ref4 =
                    (__global const ulong4 *)(B + (size_t)ref_index * BLOCK_WORDS);
                __global ulong4 *cur4 =
                    (__global ulong4 *)(B + (size_t)cur_index * BLOCK_WORDS);
                compress_inst(prev4, ref4, cur4, qi, item, pass != 0);

                prev_index = cur_index;
                cur_index += 1;
            }
        }
    }

    // Final block for the CPU's H' finalization.
    {
        __global const ulong4 *last4 =
            (__global const ulong4 *)(B + (size_t)(LANE_LENGTH - 1) * BLOCK_WORDS);
        __global ulong4 *out4 = (__global ulong4 *)(out + instance * BLOCK_WORDS);
        for (int k = item; k < BLOCK_WORDS / 4; k += ITEMS)
            out4[k] = last4[k];
    }
}
