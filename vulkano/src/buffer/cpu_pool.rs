// Copyright (c) 2017 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

// TODO: since we use some deprecated methods in there, we allow it ; remove this eventually
#![allow(deprecated)]

use smallvec::SmallVec;
use std::iter;
use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use buffer::BufferUsage;
use buffer::sys::BufferCreationError;
use buffer::sys::SparseLevel;
use buffer::sys::UnsafeBuffer;
use buffer::traits::BufferAccess;
use buffer::traits::BufferInner;
use buffer::traits::TypedBufferAccess;
use device::Device;
use device::DeviceOwned;
use device::Queue;
use instance::QueueFamily;
use memory::pool::AllocLayout;
use memory::pool::MemoryPool;
use memory::pool::MemoryPoolAlloc;
use memory::pool::StdMemoryPool;
use memory::DeviceMemoryAllocError;
use sync::AccessError;
use sync::Sharing;

use OomError;

/// Buffer from which "sub-buffers" can be individually allocated.
///
/// This buffer is especially suitable when you want to upload or download some data at each frame.
///
/// # BufferUsage
///
/// A `CpuBufferPool` is similar to a ring buffer. You start by creating an empty pool, then you
/// grab elements from the pool and use them, and if the pool is full it will automatically grow
/// in size.
///
/// Contrary to a `Vec`, elements automatically free themselves when they are dropped (ie. usually
/// when they are no longer in use by the GPU).
///
/// # Arc-like
///
/// The `CpuBufferPool` struct internally contains an `Arc`. You can clone the `CpuBufferPool` for
/// a cheap cost, and all the clones will share the same underlying buffer.
///
pub struct CpuBufferPool<T, A = Arc<StdMemoryPool>>
    where A: MemoryPool
{
    // The device of the pool.
    device: Arc<Device>,

    // The memory pool to use for allocations.
    pool: A,

    // Current buffer from which elements are grabbed.
    current_buffer: Mutex<Option<Arc<ActualBuffer<A>>>>,

    // Buffer usage.
    usage: BufferUsage,

    // Queue families allowed to access this buffer.
    queue_families: SmallVec<[u32; 4]>,

    // Necessary to make it compile.
    marker: PhantomData<Box<T>>,
}

// One buffer of the pool.
struct ActualBuffer<A>
    where A: MemoryPool
{
    // Inner content.
    inner: UnsafeBuffer,

    // The memory held by the buffer.
    memory: A::Alloc,

    // List of the chunks that are reserved.
    chunks_in_use: Mutex<Vec<ActualBufferChunk>>,

    // The index of the chunk that should be available next for the ring buffer.
    next_index: AtomicUsize,

    // Number of elements in the buffer.
    capacity: usize,
}

// Access pattern of one subbuffer.
#[derive(Debug)]
struct ActualBufferChunk {
    // First element number within the actual buffer.
    index: usize,

    // Number of occupied elements within the actual buffer.
    len: usize,

    // Number of `CpuBufferPoolSubbuffer` objects that point to this subbuffer.
    num_cpu_accesses: usize,

    // Number of `CpuBufferPoolSubbuffer` objects that point to this subbuffer and that have been
    // GPU-locked.
    num_gpu_accesses: usize,
}

/// A subbuffer allocated from a `CpuBufferPool`.
///
/// When this object is destroyed, the subbuffer is automatically reclaimed by the pool.
pub struct CpuBufferPoolSubbuffer<T, A>
    where A: MemoryPool
{
    buffer: Arc<ActualBuffer<A>>,

    // Index of the subbuffer within `buffer`. In number of elements.
    index: usize,

    // Size of the subbuffer in number of elements.
    len: usize,

    // Necessary to make it compile.
    marker: PhantomData<Box<T>>,
}

impl<T> CpuBufferPool<T> {
    /// Builds a `CpuBufferPool`.
    #[inline]
    pub fn new<'a, I>(device: Arc<Device>, usage: BufferUsage, queue_families: I)
                      -> CpuBufferPool<T>
        where I: IntoIterator<Item = QueueFamily<'a>>
    {
        unsafe { CpuBufferPool::raw(device, mem::size_of::<T>(), usage, queue_families) }
    }

    /// Builds a `CpuBufferPool` meant for simple uploads.
    ///
    /// Shortcut for a pool that can only be used as transfer source and with exclusive queue
    /// family accesses.
    #[inline]
    pub fn upload(device: Arc<Device>) -> CpuBufferPool<T> {
        CpuBufferPool::new(device, BufferUsage::transfer_source(), iter::empty())
    }

    /// Builds a `CpuBufferPool` meant for simple downloads.
    ///
    /// Shortcut for a pool that can only be used as transfer destination and with exclusive queue
    /// family accesses.
    #[inline]
    pub fn download(device: Arc<Device>) -> CpuBufferPool<T> {
        CpuBufferPool::new(device, BufferUsage::transfer_destination(), iter::empty())
    }
}

impl<T> CpuBufferPool<T> {
    #[deprecated(note = "Useless ; use new instead")]
    pub unsafe fn raw<'a, I>(device: Arc<Device>, one_size: usize, usage: BufferUsage,
                             queue_families: I) -> CpuBufferPool<T>
        where I: IntoIterator<Item = QueueFamily<'a>>
    {
        // This assertion was added after the method was deprecated. The logic of the
        // implementation doesn't hold if `one_size` is not equal to the size of `T`.
        assert_eq!(one_size, mem::size_of::<T>());

        let queue_families = queue_families
            .into_iter()
            .map(|f| f.id())
            .collect::<SmallVec<[u32; 4]>>();

        let pool = Device::standard_pool(&device);

        CpuBufferPool {
            device: device,
            pool: pool,
            current_buffer: Mutex::new(None),
            usage: usage.clone(),
            queue_families: queue_families,
            marker: PhantomData,
        }
    }

    /// Returns the current capacity of the pool, in number of elements.
    pub fn capacity(&self) -> usize {
        match *self.current_buffer.lock().unwrap() {
            None => 0,
            Some(ref buf) => buf.capacity,
        }
    }
}

impl<T, A> CpuBufferPool<T, A>
    where A: MemoryPool
{
    /// Makes sure that the capacity is at least `capacity`. Allocates memory if it is not the
    /// case.
    ///
    /// Since this can involve a memory allocation, an `OomError` can happen.
    pub fn reserve(&self, capacity: usize) -> Result<(), DeviceMemoryAllocError> {
        let mut cur_buf = self.current_buffer.lock().unwrap();

        // Check current capacity.
        match *cur_buf {
            Some(ref buf) if buf.capacity >= capacity => {
                return Ok(());
            },
            _ => (),
        };

        self.reset_buf(&mut cur_buf, capacity)
    }

    /// Grants access to a new subbuffer and puts `data` in it.
    ///
    /// If no subbuffer is available (because they are still in use by the GPU), a new buffer will
    /// automatically be allocated.
    ///
    /// > **Note**: You can think of it like a `Vec`. If you insert an element and the `Vec` is not
    /// > large enough, a new chunk of memory is automatically allocated.
    pub fn next(&self, data: T) -> CpuBufferPoolSubbuffer<T, A> {
        self.chunk(iter::once(data))
    }

    /// Grants access to a new subbuffer and puts `data` in it.
    ///
    /// If no subbuffer is available (because they are still in use by the GPU), a new buffer will
    /// automatically be allocated.
    ///
    /// > **Note**: You can think of it like a `Vec`. If you insert elements and the `Vec` is not
    /// > large enough, a new chunk of memory is automatically allocated.
    pub fn chunk<I>(&self, data: I) -> CpuBufferPoolSubbuffer<T, A>
        where I: IntoIterator<Item = T>,
              I::IntoIter: ExactSizeIterator
    {
        let data = data.into_iter();

        let mut mutex = self.current_buffer.lock().unwrap();

        let data = match self.try_next_impl(&mut mutex, data) {
            Ok(n) => return n,
            Err(d) => d,
        };

        // TODO: choose the capacity better?
        let next_capacity = data.len() * match *mutex {
            Some(ref b) => b.capacity * 2,
            None => 3,
        };

        self.reset_buf(&mut mutex, next_capacity).unwrap(); /* FIXME: propagate error */

        match self.try_next_impl(&mut mutex, data) {
            Ok(n) => n,
            Err(_) => unreachable!(),
        }
    }

    /// Grants access to a new subbuffer and puts `data` in it.
    ///
    /// Returns `None` if no subbuffer is available.
    ///
    /// A `CpuBufferPool` is always empty the first time you use it, so you shouldn't use
    /// `try_next` the first time you use it.
    #[inline]
    pub fn try_next(&self, data: T) -> Option<CpuBufferPoolSubbuffer<T, A>> {
        let mut mutex = self.current_buffer.lock().unwrap();
        self.try_next_impl(&mut mutex, iter::once(data)).ok()
    }

    // Creates a new buffer and sets it as current. The capacity is in number of elements.
    //
    // `cur_buf_mutex` must be an active lock of `self.current_buffer`.
    fn reset_buf(&self, cur_buf_mutex: &mut MutexGuard<Option<Arc<ActualBuffer<A>>>>,
                 capacity: usize)
                 -> Result<(), DeviceMemoryAllocError> {
        unsafe {
            let (buffer, mem_reqs) = {
                let sharing = if self.queue_families.len() >= 2 {
                    Sharing::Concurrent(self.queue_families.iter().cloned())
                } else {
                    Sharing::Exclusive
                };

                let size_bytes = match mem::size_of::<T>().checked_mul(capacity) {
                    Some(s) => s,
                    None => return Err(DeviceMemoryAllocError::OomError(OomError::OutOfDeviceMemory)),
                };

                match UnsafeBuffer::new(self.device.clone(),
                                          size_bytes,
                                          self.usage,
                                          sharing,
                                          SparseLevel::none()) {
                    Ok(b) => b,
                    Err(BufferCreationError::AllocError(err)) => return Err(err),
                    Err(_) => unreachable!(),        // We don't use sparse binding, therefore the other
                    // errors can't happen
                }
            };

            let mem_ty = self.device
                .physical_device()
                .memory_types()
                .filter(|t| (mem_reqs.memory_type_bits & (1 << t.id())) != 0)
                .filter(|t| t.is_host_visible())
                .next()
                .unwrap(); // Vk specs guarantee that this can't fail

            let mem = MemoryPool::alloc(&self.pool,
                                        mem_ty,
                                        mem_reqs.size,
                                        mem_reqs.alignment,
                                        AllocLayout::Linear)?;
            debug_assert!((mem.offset() % mem_reqs.alignment) == 0);
            debug_assert!(mem.mapped_memory().is_some());
            buffer.bind_memory(mem.memory(), mem.offset())?;

            **cur_buf_mutex =
                Some(Arc::new(ActualBuffer {
                                  inner: buffer,
                                  memory: mem,
                                  chunks_in_use: Mutex::new(vec![]),
                                  next_index: AtomicUsize::new(0),
                                  capacity: capacity,
                              }));

            Ok(())
        }
    }

    // Tries to lock a subbuffer from the current buffer.
    //
    // `cur_buf_mutex` must be an active lock of `self.current_buffer`.
    //
    // Returns `data` wrapped inside an `Err` if there is no slot available in the current buffer.
    fn try_next_impl<I>(&self, cur_buf_mutex: &mut MutexGuard<Option<Arc<ActualBuffer<A>>>>,
                        data: I) -> Result<CpuBufferPoolSubbuffer<T, A>, I>
        where I: ExactSizeIterator<Item = T>
    {
        // Grab the current buffer. Return `Err` if the pool wasn't "initialized" yet.
        let current_buffer = match cur_buf_mutex.clone() {
            Some(b) => b,
            None => return Err(data),
        };

        let mut chunks_in_use = current_buffer.chunks_in_use.lock().unwrap();
        let data_len = data.len();

        // Find a suitable offset, or return if none available.
        let index = {
            let next_index = {
                // Since the only place that touches `next_index` is this code, and since we
                // own a mutex lock to the buffer, it means that `next_index` can't be accessed
                // concurrently.
                // TODO: ^ eventually should be put inside the mutex
                current_buffer
                    .next_index
                    .load(Ordering::SeqCst)
            };

            // Find out whether any chunk in use overlaps this range.
            if next_index + data_len <= current_buffer.capacity &&
                !chunks_in_use.iter().any(|c| (c.index >= next_index && c.index < next_index + data_len) ||
                    (c.index <= next_index && c.index + c.len >= next_index))
            {
                next_index
            } else {
                // Impossible to allocate at `next_index`. Let's try 0 instead.
                if data_len <= current_buffer.capacity &&
                    !chunks_in_use.iter().any(|c| c.index < data_len)
                {
                    0
                } else {
                    // Buffer is full. Return.
                    return Err(data);
                }
            }
        };

        // Write `data` in the memory.
        unsafe {
            let range = (index * mem::size_of::<T>()) .. ((index + data_len) * mem::size_of::<T>());
            let mut mapping = current_buffer
                .memory
                .mapped_memory()
                .unwrap()
                .read_write::<[T]>(range);

            // TODO: assert that the data has been entirely written, in case the iterator's content didn't match the len
            for (o, i) in mapping.iter_mut().zip(data) {
                ptr::write(o, i);
            }
        }

        // Mark the chunk as in use.
        current_buffer.next_index.store(index + data_len, Ordering::SeqCst);
        chunks_in_use.push(ActualBufferChunk {
            index,
            len: data_len,
            num_cpu_accesses: 1,
            num_gpu_accesses: 0,
        });

        Ok(CpuBufferPoolSubbuffer {
               // TODO: remove .clone() once non-lexical borrows land
               buffer: current_buffer.clone(),
               index: index,
               len: data_len,
               marker: PhantomData,
           })
    }
}

// Can't automatically derive `Clone`, otherwise the compiler adds a `T: Clone` requirement.
impl<T, A> Clone for CpuBufferPool<T, A>
    where A: MemoryPool + Clone
{
    fn clone(&self) -> Self {
        let buf = self.current_buffer.lock().unwrap();

        CpuBufferPool {
            device: self.device.clone(),
            pool: self.pool.clone(),
            current_buffer: Mutex::new(buf.clone()),
            usage: self.usage.clone(),
            queue_families: self.queue_families.clone(),
            marker: PhantomData,
        }
    }
}

unsafe impl<T, A> DeviceOwned for CpuBufferPool<T, A>
    where A: MemoryPool
{
    #[inline]
    fn device(&self) -> &Arc<Device> {
        &self.device
    }
}

impl<T, A> Clone for CpuBufferPoolSubbuffer<T, A>
    where A: MemoryPool
{
    fn clone(&self) -> CpuBufferPoolSubbuffer<T, A> {
        let mut chunks_in_use_lock = self.buffer.chunks_in_use.lock().unwrap();
        let chunk = chunks_in_use_lock.iter_mut().find(|c| c.index == self.index).unwrap();

        debug_assert!(chunk.num_cpu_accesses >= 1);
        chunk.num_cpu_accesses = chunk.num_cpu_accesses.checked_add(1)
            .expect("Overflow in CPU accesses");

        CpuBufferPoolSubbuffer {
            buffer: self.buffer.clone(),
            index: self.index,
            len: self.len,
            marker: PhantomData,
        }
    }
}

unsafe impl<T, A> BufferAccess for CpuBufferPoolSubbuffer<T, A>
    where A: MemoryPool
{
    #[inline]
    fn inner(&self) -> BufferInner {
        BufferInner {
            buffer: &self.buffer.inner,
            offset: self.index * mem::size_of::<T>(),
        }
    }

    #[inline]
    fn size(&self) -> usize {
        self.len * mem::size_of::<T>()
    }

    #[inline]
    fn conflict_key(&self, _: usize, _: usize) -> u64 {
        self.buffer.inner.key() + self.index as u64
    }

    #[inline]
    fn try_gpu_lock(&self, _: bool, _: &Queue) -> Result<(), AccessError> {
        let mut chunks_in_use_lock = self.buffer.chunks_in_use.lock().unwrap();
        let chunk = chunks_in_use_lock.iter_mut().find(|c| c.index == self.index).unwrap();

        if chunk.num_gpu_accesses != 0 {
            return Err(AccessError::AlreadyInUse);
        }

        chunk.num_gpu_accesses = 1;
        Ok(())
    }

    #[inline]
    unsafe fn increase_gpu_lock(&self) {
        let mut chunks_in_use_lock = self.buffer.chunks_in_use.lock().unwrap();
        let chunk = chunks_in_use_lock.iter_mut().find(|c| c.index == self.index).unwrap();

        debug_assert!(chunk.num_gpu_accesses >= 1);
        chunk.num_gpu_accesses = chunk.num_gpu_accesses.checked_add(1)
            .expect("Overflow in GPU usages");
    }

    #[inline]
    unsafe fn unlock(&self) {
        let mut chunks_in_use_lock = self.buffer.chunks_in_use.lock().unwrap();
        let chunk = chunks_in_use_lock.iter_mut().find(|c| c.index == self.index).unwrap();

        debug_assert!(chunk.num_gpu_accesses >= 1);
        chunk.num_gpu_accesses -= 1;
    }
}

impl<T, A> Drop for CpuBufferPoolSubbuffer<T, A>
    where A: MemoryPool
{
    fn drop(&mut self) {
        let mut chunks_in_use_lock = self.buffer.chunks_in_use.lock().unwrap();
        let chunk_num = chunks_in_use_lock.iter_mut().position(|c| c.index == self.index).unwrap();

        if chunks_in_use_lock[chunk_num].num_cpu_accesses >= 2 {
            chunks_in_use_lock[chunk_num].num_cpu_accesses -= 1;
        } else {
            debug_assert_eq!(chunks_in_use_lock[chunk_num].num_gpu_accesses, 0);
            chunks_in_use_lock.remove(chunk_num);
        }
    }
}

unsafe impl<T, A> TypedBufferAccess for CpuBufferPoolSubbuffer<T, A>
    where A: MemoryPool
{
    type Content = T;
}

unsafe impl<T, A> DeviceOwned for CpuBufferPoolSubbuffer<T, A>
    where A: MemoryPool
{
    #[inline]
    fn device(&self) -> &Arc<Device> {
        self.buffer.inner.device()
    }
}

#[cfg(test)]
mod tests {
    use buffer::CpuBufferPool;
    use std::mem;

    #[test]
    fn basic_create() {
        let (device, _) = gfx_dev_and_queue!();
        let _ = CpuBufferPool::<u8>::upload(device);
    }

    #[test]
    fn reserve() {
        let (device, _) = gfx_dev_and_queue!();

        let pool = CpuBufferPool::<u8>::upload(device);
        assert_eq!(pool.capacity(), 0);

        pool.reserve(83).unwrap();
        assert_eq!(pool.capacity(), 83);
    }

    #[test]
    fn capacity_increase() {
        let (device, _) = gfx_dev_and_queue!();

        let pool = CpuBufferPool::upload(device);
        assert_eq!(pool.capacity(), 0);

        pool.next(12);
        let first_cap = pool.capacity();
        assert!(first_cap >= 1);

        for _ in 0 .. first_cap + 5 {
            mem::forget(pool.next(12));
        }

        assert!(pool.capacity() > first_cap);
    }

    #[test]
    fn reuse_subbuffers() {
        let (device, _) = gfx_dev_and_queue!();

        let pool = CpuBufferPool::upload(device);
        assert_eq!(pool.capacity(), 0);

        let mut capacity = None;
        for _ in 0 .. 64 {
            pool.next(12);

            let new_cap = pool.capacity();
            assert!(new_cap >= 1);
            match capacity {
                None => capacity = Some(new_cap),
                Some(c) => assert_eq!(c, new_cap),
            }
        }
    }
}
