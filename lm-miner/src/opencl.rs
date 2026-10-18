//! OpenCL host for the GPU Argon2id pipeline (doc 15).
//!
//! Links the system `libOpenCL` directly (`#[link(name = "OpenCL")]`) — no
//! wrapper crates, so offline builds keep working. Covers NVIDIA Pascal
//! (OpenCL 1.2 API surface only — no 2.x calls), AMD, and Intel/AMD iGPUs
//! from one codebase.
//!
//! Pipeline per batch (matches `argon2_prep`):
//! CPU: BLAKE3 seeds + H0 + initial blocks  →  GPU: fill whole batch  →
//! CPU: H' finalize + target compare.
//!
//! Verified paths: `cargo test -p lm-miner opencl` (CPU-side unit
//! tests), the ignored `gpu_kat_vectors` + 1024-case randomized battery
//! on real hardware, and `lm-miner bench --device opencl`. The kernel
//! is compiled and shipped (write-back barrier included since
//! 2026-10-18).

use std::ffi::{c_char, c_void, CString};
use std::ptr;

#[link(name = "OpenCL")]
extern "C" {
    fn clGetPlatformIDs(
        num_entries: u32,
        platforms: *mut *mut c_void,
        num_platforms: *mut u32,
    ) -> i32;
    fn clGetDeviceIDs(
        platform: *mut c_void,
        device_type: u64,
        num_entries: u32,
        devices: *mut *mut c_void,
        num_devices: *mut u32,
    ) -> i32;
    fn clCreateContext(
        properties: *const isize,
        num_devices: u32,
        devices: *const *mut c_void,
        pfn_notify: *const c_void,
        user_data: *mut c_void,
        errcode_ret: *mut i32,
    ) -> *mut c_void;
    fn clCreateCommandQueue(
        context: *mut c_void,
        device: *mut c_void,
        properties: u64,
        errcode_ret: *mut i32,
    ) -> *mut c_void;
    fn clCreateProgramWithSource(
        context: *mut c_void,
        count: u32,
        strings: *const *const c_char,
        lengths: *const usize,
        errcode_ret: *mut i32,
    ) -> *mut c_void;
    fn clBuildProgram(
        program: *mut c_void,
        num_devices: u32,
        device_list: *const *mut c_void,
        options: *const c_char,
        pfn_notify: *const c_void,
        user_data: *mut c_void,
    ) -> i32;
    fn clGetProgramBuildInfo(
        program: *mut c_void,
        device: *mut c_void,
        param_name: u32,
        param_value_size: usize,
        param_value: *mut c_void,
        param_value_size_ret: *mut usize,
    ) -> i32;
    fn clCreateKernel(
        program: *mut c_void,
        kernel_name: *const c_char,
        errcode_ret: *mut i32,
    ) -> *mut c_void;
    fn clCreateBuffer(
        context: *mut c_void,
        flags: u64,
        size: usize,
        host_ptr: *mut c_void,
        errcode_ret: *mut i32,
    ) -> *mut c_void;
    fn clSetKernelArg(
        kernel: *mut c_void,
        arg_index: u32,
        arg_size: usize,
        arg_value: *const c_void,
    ) -> i32;
    fn clEnqueueWriteBuffer(
        queue: *mut c_void,
        buffer: *mut c_void,
        blocking: u32,
        offset: usize,
        size: usize,
        ptr: *const c_void,
        num_events: u32,
        event_list: *const *mut c_void,
        event: *mut *mut c_void,
    ) -> i32;
    fn clEnqueueReadBuffer(
        queue: *mut c_void,
        buffer: *mut c_void,
        blocking: u32,
        offset: usize,
        size: usize,
        ptr: *mut c_void,
        num_events: u32,
        event_list: *const *mut c_void,
        event: *mut *mut c_void,
    ) -> i32;
    fn clEnqueueNDRangeKernel(
        queue: *mut c_void,
        kernel: *mut c_void,
        work_dim: u32,
        global_offset: *const usize,
        global_size: *const usize,
        local_size: *const usize,
        num_events: u32,
        event_list: *const *mut c_void,
        event: *mut *mut c_void,
    ) -> i32;
    fn clFinish(queue: *mut c_void) -> i32;
    fn clWaitForEvents(num_events: u32, event_list: *const *mut c_void) -> i32;
    fn clReleaseEvent(event: *mut c_void) -> i32;
    fn clReleaseMemObject(m: *mut c_void) -> i32;
    fn clReleaseKernel(k: *mut c_void) -> i32;
    fn clReleaseProgram(p: *mut c_void) -> i32;
    fn clReleaseCommandQueue(q: *mut c_void) -> i32;
    fn clReleaseContext(c: *mut c_void) -> i32;
    fn clGetDeviceInfo(
        device: *mut c_void,
        param_name: u32,
        param_value_size: usize,
        param_value: *mut c_void,
        param_value_size_ret: *mut usize,
    ) -> i32;
    fn clGetKernelWorkGroupInfo(
        kernel: *mut c_void,
        device: *mut c_void,
        param_name: u32,
        param_value_size: usize,
        param_value: *mut c_void,
        param_value_size_ret: *mut usize,
    ) -> i32;
}

const CL_SUCCESS: i32 = 0;
const CL_DEVICE_TYPE_GPU: u64 = 1 << 2;
const CL_DEVICE_TYPE_ALL: u64 = 0xFFFF_FFFF;
const CL_MEM_READ_ONLY: u64 = 1 << 0;
const CL_MEM_WRITE_ONLY: u64 = 1 << 1;
const CL_MEM_READ_WRITE: u64 = 1 << 2;
const CL_DEVICE_NAME: u32 = 0x102B;
const CL_PROGRAM_BUILD_LOG: u32 = 0x1183;
const CL_TRUE: u32 = 1;
const CL_FALSE: u32 = 0;

fn ck(code: i32, what: &str) -> Result<(), String> {
    if code == CL_SUCCESS {
        Ok(())
    } else {
        Err(format!("{what} failed (OpenCL error {code})"))
    }
}

/// An opened GPU: context + queue + compiled fill kernel.
pub struct GpuDevice {
    context: *mut c_void,
    queue: *mut c_void,
    program: *mut c_void,
    kernel: *mut c_void,
    device: *mut c_void,
    name: String,
}

impl GpuDevice {
    /// Raw cl_device_id (for future queries/telemetry).
    pub fn raw_device(&self) -> *mut c_void {
        self.device
    }
}

// OpenCL handles are context-bound and this type is used from one thread.
unsafe impl Send for GpuDevice {}

impl GpuDevice {
    /// Open platform `platform_idx`, GPU `device_idx`, compile the kernel.
    /// Falls back to any device kind when no GPU enumerates (iGPUs on odd
    /// drivers sometimes report as non-GPU).
    pub fn open(platform_idx: usize, device_idx: usize) -> Result<Self, String> {
        let mut n_platforms = 0u32;
        ck(
            unsafe { clGetPlatformIDs(0, ptr::null_mut(), &mut n_platforms) },
            "clGetPlatformIDs(count)",
        )?;
        if n_platforms == 0 {
            return Err("no OpenCL platforms found".to_string());
        }
        let mut platforms = vec![ptr::null_mut(); n_platforms as usize];
        ck(
            unsafe { clGetPlatformIDs(n_platforms, platforms.as_mut_ptr(), ptr::null_mut()) },
            "clGetPlatformIDs(list)",
        )?;
        let platform = *platforms
            .get(platform_idx)
            .ok_or_else(|| format!("platform {platform_idx} missing (have {})", platforms.len()))?;

        let dev = Self::pick_device(platform, device_idx)?;
        let mut err = 0i32;
        let context = unsafe {
            clCreateContext(
                ptr::null(),
                1,
                &dev as *const *mut c_void,
                ptr::null(),
                ptr::null_mut(),
                &mut err,
            )
        };
        ck(err, "clCreateContext")?;
        if context.is_null() {
            return Err("clCreateContext returned null".to_string());
        }

        let queue = unsafe { clCreateCommandQueue(context, dev, 0, &mut err) };
        if err != CL_SUCCESS {
            unsafe { clReleaseContext(context) };
            return Err(format!("clCreateCommandQueue failed ({err})"));
        }
        let source = CString::new(include_str!("../kernels/argon2id_fill.cl"))
            .map_err(|e| format!("kernel source has NUL: {e}"))?;
        let src_ptr = source.as_ptr();
        let src_len = source.as_bytes().len();
        let program =
            unsafe { clCreateProgramWithSource(context, 1, &src_ptr, &src_len, &mut err) };
        if err != CL_SUCCESS {
            unsafe {
                clReleaseCommandQueue(queue);
                clReleaseContext(context);
            }
            return Err(format!("clCreateProgramWithSource failed ({err})"));
        }
        let rc = unsafe {
            clBuildProgram(
                program,
                1,
                &dev as *const *mut c_void,
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
            )
        };
        if rc != CL_SUCCESS {
            let log = Self::build_log(program, dev);
            unsafe {
                clReleaseProgram(program);
                clReleaseCommandQueue(queue);
                clReleaseContext(context);
            }
            return Err(format!("clBuildProgram failed ({rc}):\n{log}"));
        }
        let kname = CString::new("argon2id_fill").expect("static");
        let kernel = unsafe { clCreateKernel(program, kname.as_ptr(), &mut err) };
        if err != CL_SUCCESS {
            unsafe {
                clReleaseProgram(program);
                clReleaseCommandQueue(queue);
                clReleaseContext(context);
            }
            return Err(format!("clCreateKernel(argon2id_fill) failed ({err})"));
        }
        let name = Self::device_name(dev);
        Ok(Self {
            context,
            queue,
            program,
            kernel,
            device: dev,
            name,
        })
    }

/// Enumerate OpenCL devices as `(platform index, device index, name)` —
/// what GUI pickers and `gpu-test` need without opening a context.
/// Mirrors [`GpuDevice::open`]'s traversal (GPU kind first, then all kinds
/// for odd iGPU drivers).
pub fn list_devices() -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut n_platforms = 0u32;
    if unsafe { clGetPlatformIDs(0, ptr::null_mut(), &mut n_platforms) } != CL_SUCCESS
        || n_platforms == 0
    {
        return out;
    }
    let mut platforms = vec![ptr::null_mut(); n_platforms as usize];
    if unsafe { clGetPlatformIDs(n_platforms, platforms.as_mut_ptr(), ptr::null_mut()) }
        != CL_SUCCESS
    {
        return out;
    }
    for (p, platform) in platforms.iter().enumerate() {
        for kind in [CL_DEVICE_TYPE_GPU, CL_DEVICE_TYPE_ALL] {
            let mut n = 0u32;
            if unsafe { clGetDeviceIDs(*platform, kind, 0, ptr::null_mut(), &mut n) } != CL_SUCCESS
                || n == 0
            {
                continue;
            }
            let mut devs = vec![ptr::null_mut(); n as usize];
            if unsafe {
                clGetDeviceIDs(*platform, kind, n, devs.as_mut_ptr(), ptr::null_mut())
            } != CL_SUCCESS
            {
                continue;
            }
            for (d, dev) in devs.iter().enumerate() {
                out.push((p, d, GpuDevice::device_name(*dev)));
            }
            break; // GPU kind covered this platform
        }
    }
    out
}

    fn pick_device(platform: *mut c_void, idx: usize) -> Result<*mut c_void, String> {
        for kind in [CL_DEVICE_TYPE_GPU, CL_DEVICE_TYPE_ALL] {
            let mut n = 0u32;
            if unsafe { clGetDeviceIDs(platform, kind, 0, ptr::null_mut(), &mut n) } != CL_SUCCESS
                || n == 0
            {
                continue;
            }
            let mut devs = vec![ptr::null_mut(); n as usize];
            ck(
                unsafe { clGetDeviceIDs(platform, kind, n, devs.as_mut_ptr(), ptr::null_mut()) },
                "clGetDeviceIDs(list)",
            )?;
            if let Some(&d) = devs.get(idx) {
                return Ok(d);
            }
            return Err(format!(
                "device {idx} missing (platform has {})",
                devs.len()
            ));
        }
        Err("no OpenCL devices found on platform".to_string())
    }

    fn device_name(dev: *mut c_void) -> String {
        let mut size = 0usize;
        if unsafe { clGetDeviceInfo(dev, CL_DEVICE_NAME, 0, ptr::null_mut(), &mut size) }
            != CL_SUCCESS
            || size == 0
            || size > 4096
        {
            return "(unknown device)".to_string();
        }
        let mut buf = vec![0u8; size];
        if unsafe {
            clGetDeviceInfo(
                dev,
                CL_DEVICE_NAME,
                size,
                buf.as_mut_ptr() as *mut c_void,
                ptr::null_mut(),
            )
        } != CL_SUCCESS
        {
            return "(unknown device)".to_string();
        }
        String::from_utf8_lossy(&buf)
            .trim_end_matches('\0')
            .to_string()
    }

    fn build_log(program: *mut c_void, dev: *mut c_void) -> String {
        let mut size = 0usize;
        if unsafe {
            clGetProgramBuildInfo(
                program,
                dev,
                CL_PROGRAM_BUILD_LOG,
                0,
                ptr::null_mut(),
                &mut size,
            )
        } != CL_SUCCESS
            || size == 0
            || size > 1 << 20
        {
            return "(no build log)".to_string();
        }
        let mut buf = vec![0u8; size];
        unsafe {
            clGetProgramBuildInfo(
                program,
                dev,
                CL_PROGRAM_BUILD_LOG,
                size,
                buf.as_mut_ptr() as *mut c_void,
                ptr::null_mut(),
            )
        };
        String::from_utf8_lossy(&buf).into_owned()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Kernel build facts that predict occupancy: the max work-group size
    /// the compiler allows (register/private-memory limited) and how much
    /// private memory each work-item spills. Diagnostic only.
    pub fn kernel_limits(&self) -> (usize, usize) {
        let mut wg = 0usize;
        let ok1 = unsafe {
            clGetKernelWorkGroupInfo(
                self.kernel,
                self.device,
                0x11B0, // CL_KERNEL_WORK_GROUP_SIZE
                std::mem::size_of::<usize>(),
                &mut wg as *mut usize as *mut c_void,
                ptr::null_mut(),
            )
        } == CL_SUCCESS;
        let mut pm = 0usize;
        let ok2 = unsafe {
            clGetKernelWorkGroupInfo(
                self.kernel,
                self.device,
                0x11B8, // CL_KERNEL_PRIVATE_MEM_SIZE
                std::mem::size_of::<usize>(),
                &mut pm as *mut usize as *mut c_void,
                ptr::null_mut(),
            )
        } == CL_SUCCESS;
        (if ok1 { wg } else { 0 }, if ok2 { pm } else { 0 })
    }

    /// Persistent launch buffers for one batch capacity: created once and
    /// reused across launches (the per-launch clCreateBuffer set — a
    /// batch x 1 MiB mem buffer among them — is pure driver churn in the
    /// grind loop). Capacity is in unpadded instances; every launch with
    /// the same batch size reuses the same objects.
    pub fn create_batch_buffers(&self, capacity: usize) -> Result<BatchBuffers, String> {
        let n_pad = capacity.div_ceil(4) * 4;
        let words_per_instance = 1024usize * 128;
        let mut err = 0i32;
        let init = unsafe {
            clCreateBuffer(self.context, CL_MEM_READ_ONLY, n_pad * 2 * 128 * 8, ptr::null_mut(), &mut err)
        };
        ck(err, "clCreateBuffer(init)")?;
        let mem = unsafe {
            clCreateBuffer(
                self.context,
                CL_MEM_READ_WRITE,
                n_pad * words_per_instance * 8,
                ptr::null_mut(),
                &mut err,
            )
        };
        if err != CL_SUCCESS {
            unsafe { clReleaseMemObject(init) };
            return Err(format!("clCreateBuffer(mem) failed ({err})"));
        }
        let out = unsafe {
            clCreateBuffer(self.context, CL_MEM_WRITE_ONLY, n_pad * 128 * 8, ptr::null_mut(), &mut err)
        };
        if err != CL_SUCCESS {
            unsafe {
                clReleaseMemObject(init);
                clReleaseMemObject(mem);
            }
            return Err(format!("clCreateBuffer(out) failed ({err})"));
        }
        let scratch = unsafe {
            clCreateBuffer(self.context, CL_MEM_READ_WRITE, n_pad * 384 * 8, ptr::null_mut(), &mut err)
        };
        if err != CL_SUCCESS {
            unsafe {
                clReleaseMemObject(init);
                clReleaseMemObject(mem);
                clReleaseMemObject(out);
            }
            return Err(format!("clCreateBuffer(scratch) failed ({err})"));
        }
        Ok(BatchBuffers {
            init,
            mem,
            out,
            scratch,
            capacity,
        })
    }

    /// Same contract as [`fill_batch`] but over CALLER-OWNED persistent
    /// buffers ([`create_batch_buffers`]); `bufs.capacity` must be >=
    /// `inits.len()`.
    pub fn fill_batch_with(
        &self,
        bufs: &BatchBuffers,
        inits: &[[[u64; 128]; 2]],
    ) -> Result<Vec<[u64; 128]>, String> {
        let n = inits.len();
        if n == 0 {
            return Err("empty batch".to_string());
        }
        if n > bufs.capacity {
            return Err(format!("batch {n} exceeds buffer capacity {}", bufs.capacity));
        }
        const LOCAL: usize = 32; // 4 instances x 8 items
        let n_pad = bufs.capacity.div_ceil(4) * 4;
        let mut init_flat = Vec::with_capacity(n_pad * 2 * 128);
        for pair in inits {
            init_flat.extend_from_slice(&pair[0]);
            init_flat.extend_from_slice(&pair[1]);
        }
        init_flat.resize(n_pad * 2 * 128, 0);
        ck(
            unsafe {
                clEnqueueWriteBuffer(
                    self.queue,
                    bufs.init,
                    CL_TRUE,
                    0,
                    init_flat.len() * 8,
                    init_flat.as_ptr() as *const c_void,
                    0,
                    ptr::null(),
                    ptr::null_mut(),
                )
            },
            "clEnqueueWriteBuffer(init)",
        )?;
        for (i, buf) in [bufs.init, bufs.mem, bufs.out, bufs.scratch].iter().enumerate() {
            let mut b = *buf;
            ck(
                unsafe { clSetKernelArg(self.kernel, i as u32, 8, &mut b as *mut _ as *const c_void) },
                "clSetKernelArg",
            )?;
        }
        let global = n_pad * 8; // 8 items per instance
        let local = LOCAL;
        ck(
            unsafe {
                clEnqueueNDRangeKernel(
                    self.queue,
                    self.kernel,
                    1,
                    ptr::null(),
                    &global as *const usize,
                    &local as *const usize,
                    0,
                    ptr::null(),
                    ptr::null_mut(),
                )
            },
            "clEnqueueNDRangeKernel",
        )?;
        ck(unsafe { clFinish(self.queue) }, "clFinish")?;
        let mut out_flat = vec![0u64; n_pad * 128];
        ck(
            unsafe {
                clEnqueueReadBuffer(
                    self.queue,
                    bufs.out,
                    CL_TRUE,
                    0,
                    out_flat.len() * 8,
                    out_flat.as_mut_ptr() as *mut c_void,
                    0,
                    ptr::null(),
                    ptr::null_mut(),
                )
            },
            "clEnqueueReadBuffer(out)",
        )?;
        let mut out = Vec::with_capacity(n);
        for chunk in out_flat.chunks_exact(128).take(n) {
            let mut block = [0u64; 128];
            block.copy_from_slice(chunk);
            out.push(block);
        }
        Ok(out)
    }

    /// Enqueue a batch WITHOUT waiting (the pipelined hot path): async
    /// init write, async fill, async result read, all on the one in-order
    /// queue. Returns a [`PendingBatch`]; the caller typically enqueues
    /// the NEXT batch before waiting on this one, so the GPU never idles
    /// between fills while the CPU preps and finalizes.
    ///
    /// In-order-queue safety: every buffer is touched in submission order
    /// (write N, fill N, read N, write N+1, fill N+1, ...), so a single
    /// [`BatchBuffers`] set needs no double buffering on the device side.
    /// Kernel args are captured at enqueue time per the OpenCL spec, so
    /// re-argging the shared kernel while a fill is in flight is safe.
    pub fn submit_batch(
        &self,
        bufs: &BatchBuffers,
        inits: &[[[u64; 128]; 2]],
    ) -> Result<PendingBatch, String> {
        let n = inits.len();
        if n == 0 {
            return Err("empty batch".to_string());
        }
        if n > bufs.capacity {
            return Err(format!("batch {n} exceeds buffer capacity {}", bufs.capacity));
        }
        const LOCAL: usize = 32; // 4 instances x 8 items
        let n_pad = bufs.capacity.div_ceil(4) * 4;
        let mut init_flat = Vec::with_capacity(n_pad * 2 * 128);
        for pair in inits {
            init_flat.extend_from_slice(&pair[0]);
            init_flat.extend_from_slice(&pair[1]);
        }
        init_flat.resize(n_pad * 2 * 128, 0);
        let mut out_flat = vec![0u64; n_pad * 128];
        // Host pointers must stay valid until the commands execute: both
        // vectors are moved into the PendingBatch and freed only after
        // its event is waited on (Drop).
        let init_ptr = init_flat.as_ptr() as *const c_void;
        let out_ptr = out_flat.as_mut_ptr() as *mut c_void;
        ck(
            unsafe {
                clEnqueueWriteBuffer(
                    self.queue,
                    bufs.init,
                    CL_FALSE,
                    0,
                    init_flat.len() * 8,
                    init_ptr,
                    0,
                    ptr::null(),
                    ptr::null_mut(),
                )
            },
            "clEnqueueWriteBuffer(init)",
        )?;
        for (i, buf) in [bufs.init, bufs.mem, bufs.out, bufs.scratch].iter().enumerate() {
            let mut b = *buf;
            ck(
                unsafe { clSetKernelArg(self.kernel, i as u32, 8, &mut b as *mut _ as *const c_void) },
                "clSetKernelArg",
            )?;
        }
        let global = n_pad * 8;
        let local = LOCAL;
        ck(
            unsafe {
                clEnqueueNDRangeKernel(
                    self.queue,
                    self.kernel,
                    1,
                    ptr::null(),
                    &global as *const usize,
                    &local as *const usize,
                    0,
                    ptr::null(),
                    ptr::null_mut(),
                )
            },
            "clEnqueueNDRangeKernel",
        )?;
        let mut event: *mut c_void = ptr::null_mut();
        ck(
            unsafe {
                clEnqueueReadBuffer(
                    self.queue,
                    bufs.out,
                    CL_FALSE,
                    0,
                    out_flat.len() * 8,
                    out_ptr,
                    0,
                    ptr::null(),
                    &mut event as *mut *mut c_void,
                )
            },
            "clEnqueueReadBuffer(out)",
        )?;
        Ok(PendingBatch {
            event,
            n,
            init_flat,
            out_flat,
        })
    }

    /// Run the fill for a batch of instances. `inits[i]` = the two initial
    /// 128-word blocks for instance i (from `argon2_prep::initial_blocks`).
    /// Returns the 128-word final block per instance. Batch sizing is the
    /// caller's job (VRAM/1 MiB per instance; keep launches under the iGPU
    /// watchdog, doc 15 §15.2).
    ///
    /// v2 pads the batch to a multiple of 4 instances (one workgroup = 32
    /// items = 4 instances; every workgroup is fully populated so barrier
    /// participation is uniform) and gives each instance a 3-block global
    /// scratch array (input | mid | address). One-shot buffer set — the
    /// grind loop uses [`fill_batch_with`] with persistent buffers.
    pub fn fill_batch(&self, inits: &[[[u64; 128]; 2]]) -> Result<Vec<[u64; 128]>, String> {
        let n = inits.len();
        if n == 0 {
            return Err("empty batch".to_string());
        }
        const LOCAL: usize = 32; // 4 instances x 8 items
        let n_pad = n.div_ceil(4) * 4;
        let words_per_instance = 1024usize * 128;
        let mut init_flat = Vec::with_capacity(n_pad * 2 * 128);
        for pair in inits {
            init_flat.extend_from_slice(&pair[0]);
            init_flat.extend_from_slice(&pair[1]);
        }
        init_flat.resize(n_pad * 2 * 128, 0);
        let mut err = 0i32;
        let init_buf = unsafe {
            clCreateBuffer(
                self.context,
                CL_MEM_READ_ONLY,
                init_flat.len() * 8,
                ptr::null_mut(),
                &mut err,
            )
        };
        ck(err, "clCreateBuffer(init)")?;
        let mem_buf = unsafe {
            clCreateBuffer(
                self.context,
                CL_MEM_READ_WRITE,
                n_pad * words_per_instance * 8,
                ptr::null_mut(),
                &mut err,
            )
        };
        if err != CL_SUCCESS {
            unsafe { clReleaseMemObject(init_buf) };
            return Err(format!("clCreateBuffer(mem) failed ({err})"));
        }
        let out_buf = unsafe {
            clCreateBuffer(
                self.context,
                CL_MEM_WRITE_ONLY,
                n_pad * 128 * 8,
                ptr::null_mut(),
                &mut err,
            )
        };
        if err != CL_SUCCESS {
            unsafe {
                clReleaseMemObject(init_buf);
                clReleaseMemObject(mem_buf);
            }
            return Err(format!("clCreateBuffer(out) failed ({err})"));
        }
        // Per-instance scratch: input | mid | address (3 x 128 words).
        let scratch_buf = unsafe {
            clCreateBuffer(
                self.context,
                CL_MEM_READ_WRITE,
                n_pad * 384 * 8,
                ptr::null_mut(),
                &mut err,
            )
        };
        if err != CL_SUCCESS {
            unsafe {
                clReleaseMemObject(init_buf);
                clReleaseMemObject(mem_buf);
                clReleaseMemObject(out_buf);
            }
            return Err(format!("clCreateBuffer(scratch) failed ({err})"));
        }
        let result = (|| {
            ck(
                unsafe {
                    clEnqueueWriteBuffer(
                        self.queue,
                        init_buf,
                        CL_TRUE,
                        0,
                        init_flat.len() * 8,
                        init_flat.as_ptr() as *const c_void,
                        0,
                        ptr::null(),
                        ptr::null_mut(),
                    )
                },
                "clEnqueueWriteBuffer(init)",
            )?;
            for (i, buf) in [init_buf, mem_buf, out_buf, scratch_buf].iter().enumerate() {
                let mut b = *buf;
                ck(
                    unsafe {
                        clSetKernelArg(self.kernel, i as u32, 8, &mut b as *mut _ as *const c_void)
                    },
                    "clSetKernelArg",
                )?;
            }
            let global = n_pad * 8; // 8 items per instance
            let local = LOCAL;
            ck(
                unsafe {
                    clEnqueueNDRangeKernel(
                        self.queue,
                        self.kernel,
                        1,
                        ptr::null(),
                        &global as *const usize,
                        &local as *const usize,
                        0,
                        ptr::null(),
                        ptr::null_mut(),
                    )
                },
                "clEnqueueNDRangeKernel",
            )?;
            ck(unsafe { clFinish(self.queue) }, "clFinish")?;
            let mut out_flat = vec![0u64; n_pad * 128];
            ck(
                unsafe {
                    clEnqueueReadBuffer(
                        self.queue,
                        out_buf,
                        CL_TRUE,
                        0,
                        out_flat.len() * 8,
                        out_flat.as_mut_ptr() as *mut c_void,
                        0,
                        ptr::null(),
                        ptr::null_mut(),
                    )
                },
                "clEnqueueReadBuffer(out)",
            )?;
            let mut out = Vec::with_capacity(n);
            for chunk in out_flat.chunks_exact(128).take(n) {
                let mut block = [0u64; 128];
                block.copy_from_slice(chunk);
                out.push(block);
            }
            Ok(out)
        })();
        unsafe {
            clReleaseMemObject(init_buf);
            clReleaseMemObject(mem_buf);
            clReleaseMemObject(out_buf);
            clReleaseMemObject(scratch_buf);
        }
        result
    }
}

/// One asynchronously-submitted batch (see [`GpuDevice::submit_batch`]).
/// Owns the host staging buffers (the queue's write/read pointers must
/// stay valid until execution) and the completion event.
pub struct PendingBatch {
    event: *mut c_void,
    n: usize,
    #[allow(dead_code)]
    init_flat: Vec<u64>,
    out_flat: Vec<u64>,
}

impl PendingBatch {
    /// Wait for the batch and take the per-instance final blocks. The
    /// host buffers free here (after the event), the event releases on
    /// Drop.
    pub fn wait_and_take(self) -> Result<Vec<[u64; 128]>, String> {
        ck(unsafe { clWaitForEvents(1, &self.event as *const *mut c_void) }, "clWaitForEvents")?;
        let n = self.n;
        let mut out = Vec::with_capacity(n);
        for chunk in self.out_flat.chunks_exact(128).take(n) {
            let mut block = [0u64; 128];
            block.copy_from_slice(chunk);
            out.push(block);
        }
        Ok(out)
    }
}

impl Drop for PendingBatch {
    fn drop(&mut self) {
        // The queue's async write/read may still reference this batch's
        // host buffers: WAIT before they free (early-stop and panic paths
        // come through here), then release the event.
        if !self.event.is_null() {
            unsafe {
                let _ = clWaitForEvents(1, &self.event as *const *mut c_void);
                clReleaseEvent(self.event);
            }
        }
    }
}

/// Persistent OpenCL launch buffers (see [`GpuDevice::create_batch_buffers`]).
pub struct BatchBuffers {
    init: *mut c_void,
    mem: *mut c_void,
    out: *mut c_void,
    scratch: *mut c_void,
    capacity: usize,
}

impl Drop for BatchBuffers {
    fn drop(&mut self) {
        unsafe {
            clReleaseMemObject(self.init);
            clReleaseMemObject(self.mem);
            clReleaseMemObject(self.out);
            clReleaseMemObject(self.scratch);
        }
    }
}

impl Drop for GpuDevice {
    fn drop(&mut self) {
        unsafe {
            clReleaseKernel(self.kernel);
            clReleaseProgram(self.program);
            clReleaseCommandQueue(self.queue);
            clReleaseContext(self.context);
        }
    }
}

/// KAT gate over real hardware: prep on CPU, fill on the GPU, compare
/// against `lm-pow`'s frozen mem values. Run with a GPU present; the
/// ignored test below documents the exact invocation.
pub fn gpu_kat_vectors(dev: &GpuDevice) -> Result<(), String> {
    use crate::argon2_prep;
    let header: Vec<u8> = (0u8..80).collect();
    let cases: [(u64, u64); 3] = [(0, 1), (1, 1), (u64::MAX, 12345)];
    for (nonce, height) in cases {
        let f = lm_pow::pow_full(&header, nonce, height);
        let h0 = argon2_prep::h0(&f.seed, &argon2_prep::lucky_salt(height), b"lowmine-pow/v1");
        let pair = argon2_prep::initial_blocks(&h0);
        let outs = dev.fill_batch(std::slice::from_ref(&pair))?;
        let mut last_bytes = [0u8; 1024];
        for (i, w) in outs[0].iter().enumerate() {
            last_bytes[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        // H' finalize on CPU (same helper the CPU path uses).
        let tag = argon2_prep::blake2b_long(&[&last_bytes], 32);
        let mut mine = [0u8; 32];
        mine.copy_from_slice(&tag);
        if mine != f.mem {
            return Err(format!(
                "GPU/CPU mem mismatch at nonce={nonce} height={height}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_paths_without_hardware() {
        // Opening a nonsense platform must fail cleanly, not crash.
        assert!(GpuDevice::open(usize::MAX, 0).is_err());
    }

    /// Hardware gate — run explicitly with a GPU present:
    /// `GPU_TESTS=1 cargo test -p lm-miner -- --ignored`
    /// (plus `lm-miner gpu-test` for the CLI form).
    #[test]
    #[ignore]
    fn gpu_kat_vectors_on_hardware() {
        if std::env::var("GPU_TESTS").is_err() {
            eprintln!("skipping: set GPU_TESTS=1 with OpenCL hardware present");
            return;
        }
        let dev = GpuDevice::open(0, 0).expect("open default GPU");
        eprintln!("testing on {}", dev.name());
        gpu_kat_vectors(&dev).expect("GPU mem must equal lm-pow mem");
    }
}
