//! Interfaces with a buffer.

// [TEMP]:
#![allow(dead_code)]

use std;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

use ffi::cl_GLuint;

use core::{self, OclPrm, Mem as MemCore, CommandQueue, MemFlags, MemInfo, MemInfoResult,
    ClEventPtrNew, ClWaitList, BufferRegion, MappedMem as MappedMemCore, Event as EventCore,
    EventList as EventListCore, MapFlags};
use core::error::{Error as OclError, Result as OclResult};
use standard::{Queue, MemLen, SpatialDims, AsMemRef};


fn check_len(mem_len: usize, data_len: usize, offset: usize) -> OclResult<()> {
    if offset >= mem_len {
        OclError::err(format!("ocl::Buffer::enq(): Offset out of range. \
            (mem_len: {}, data_len: {}, offset: {}", mem_len, data_len, offset))
    } else if data_len > (mem_len - offset) {
        OclError::err("ocl::Buffer::enq(): Data length exceeds buffer length.")
    } else {
        Ok(())
    }
}


/// Information about what to do when `MappedMem` goes out of scope.
enum DelayedUnmap {
    Some {
        queue: CommandQueue,
        mem: MemCore,
        ewait: Option<EventListCore>,
        enew: Option<EventCore>,
    },
    None,
}


/// A view of mapped memory.
///
/// ### [UNSTABLE]
///
/// Still in a state of flux.
///
pub struct MappedMem<T: OclPrm> {
    core: MappedMemCore<T>,
    delayed_unmap: DelayedUnmap,
}

impl<T: OclPrm> MappedMem<T> {
    /// Returns a new `MappedMem`.
    pub fn new(core: MappedMemCore<T>) -> MappedMem<T> {
        MappedMem {
            core: core,
            delayed_unmap: DelayedUnmap::None,
        }
    }

    /// Automatically unmaps when this `MappedMem` goes out of scope or by
    /// calling `::unmap`.
    ///
    /// Automatically adds the event associated with the original map event to
    /// the wait event list.
    ///
    pub fn unmap_later<EVL, EV>(&mut self, queue: CommandQueue, mem: MemCore,
            wait_list: Option<EVL>, new_event: Option<EV>)
            where EVL: Into<EventListCore>, EV: Into<EventCore>
    {
        self.delayed_unmap = DelayedUnmap::Some {
            queue: queue,
            mem: mem,
            ewait: wait_list.map(|evl| evl.into()),
            enew: new_event.map(|ev| ev.into()),
        }
    }

    /// Sets events for a `MappedMem` which has already had `::unmap_later`
    /// called first.
    pub fn set_unmap_events<'a, EVL, EV>(&'a mut self, wait_list: Option<EVL>, new_event: Option<EV>)
            -> &'a mut MappedMem<T>
            where EVL: Into<EventListCore>, EV: Into<EventCore>
    {
        assert!(!self.core.is_unmapped(), "ocl::MappedMem::unmap: \
            This 'MappedMem' is already unmapped.");

        match self.delayed_unmap {
            DelayedUnmap::Some { ref mut ewait, ref mut enew, .. } => {
                if let Some(evl) = wait_list {
                    *ewait = Some(evl.into())
                }

                if let Some(ev) = new_event {
                    *enew = Some(ev.into())
                }
            },
            DelayedUnmap::None => panic!("ocl::MappedMem::set_unmap_events: Can only be called \
                after '::unmap_later' has already been called."),
        }

        self
    }

    pub fn unmap(&mut self) -> OclResult<()> {
        if self.core.is_unmapped() { return Err("ocl::MappedMem::unmap: \
            This 'MappedMem' is already unmapped.".into()); }

        match self.delayed_unmap {
            DelayedUnmap::Some { ref queue, ref mem, ref ewait, ref mut enew } => {
                self.core.unmap_mem_object(queue, mem,
                    match *ewait {
                        Some(ref el) => Some(el as &ClWaitList),
                        None => None,
                    },
                    match *enew {
                        Some(ref mut e) => Some(e as &mut ClEventPtrNew),
                        None => None,
                    },
                )
            },
            DelayedUnmap::None => Err("ocl::MappedMem::set_unmap_events: Can only be called \
                after '::unmap_later' has already been called.".into()),
        }
    }
}

impl<T> Deref for MappedMem<T> where T: OclPrm {
    type Target = MappedMemCore<T>;

    fn deref(&self) -> &MappedMemCore<T> {
        &self.core
    }
}

impl<T> DerefMut for MappedMem<T> where T: OclPrm {
    fn deref_mut(&mut self) -> &mut MappedMemCore<T> {
        &mut self.core
    }
}

impl<T> Drop for MappedMem<T> where T: OclPrm {
    fn drop(&mut self) {
        if self.core.is_unmapped() { return; }

        match self.delayed_unmap {
            DelayedUnmap::Some { ref queue, ref mem, ref ewait, ref mut enew } => {
                self.core.unmap_mem_object(queue, mem,
                    match *ewait {
                        Some(ref el) => Some(el as &ClWaitList),
                        None => None,
                    },
                    match *enew {
                        Some(ref mut e) => Some(e as &mut ClEventPtrNew),
                        None => None,
                    },
                ).unwrap()
            },
            DelayedUnmap::None => (),
        }
    }
}


/// The type of operation to be performed by a command.
pub enum BufferCmdKind<'b, T: 'b> {
    Unspecified,
    Read { data: &'b mut [T] },
    Write { data: &'b [T] },
    Map { flags: Option<MapFlags>, len: Option<usize> },
    Copy { dst_buffer: &'b MemCore, dst_offset: Option<usize>, len: Option<usize> },
    Fill { pattern: T, len: Option<usize> },
    CopyToImage { image: &'b MemCore, dst_origin: [usize; 3], region: [usize; 3] },
    GLAcquire,
    GLRelease,
}

impl<'b, T: 'b> BufferCmdKind<'b, T> {
    fn is_unspec(&'b self) -> bool {
        if let BufferCmdKind::Unspecified = *self {
            true
        } else {
            false
        }
    }
}

/// The 'shape' of the data to be processed, whether one or multi-dimensional.
///
/// Should really be called dimensionality or something.
///
pub enum BufferCmdDataShape {
    Lin { offset: usize },
    Rect {
        src_origin: [usize; 3],
        dst_origin: [usize; 3],
        region: [usize; 3],
        src_row_pitch: usize,
        src_slc_pitch: usize,
        dst_row_pitch: usize,
        dst_slc_pitch: usize,
    },
}

/// A buffer command builder used to enqueue reads, writes, fills, and copies.
///
/// Create one by using `Buffer::cmd` or with shortcut methods such as
/// `Buffer::read` and `Buffer::write`.
///
/// ## Examples
///
/// ```text
/// // Copies one buffer to another:
/// src_buffer.cmd().copy(&dst_buffer, 0, dst_buffer.len()).enq().unwrap();
///
/// // Writes from a vector to an buffer, waiting on an event:
/// buffer.write(&src_vec).ewait(&event).enq().unwrap();
///
/// // Reads from a buffer into a vector, waiting on an event list and
/// // filling a new empty event:
/// buffer.read(&dst_vec).ewait(&event_list).enew(&empty_event).enq().unwrap();
///
/// // Reads without blocking:
/// buffer.cmd().read_async(&dst_vec).enew(&empty_event).enq().unwrap();
///
/// ```
///
pub struct BufferCmd<'b, T: 'b + OclPrm> {
    queue: &'b Queue,
    obj_core: &'b MemCore,
    block: bool,
    lock_block: bool,
    kind: BufferCmdKind<'b, T>,
    shape: BufferCmdDataShape,
    ewait: Option<&'b ClWaitList>,
    enew: Option<&'b mut ClEventPtrNew>,
    mem_len: usize,
}

/// [UNSTABLE]: All methods still in a state of tweakification.
impl<'b, T: 'b + OclPrm> BufferCmd<'b, T> {
    /// Returns a new buffer command builder associated with with the
    /// memory object `obj_core` along with a default `queue` and `mem_len`
    /// (the length of the device side buffer).
    pub fn new(queue: &'b Queue, obj_core: &'b MemCore, mem_len: usize)
            -> BufferCmd<'b, T>
    {
        BufferCmd {
            queue: queue,
            obj_core: obj_core,
            block: true,
            lock_block: false,
            kind: BufferCmdKind::Unspecified,
            shape: BufferCmdDataShape::Lin { offset: 0 },
            ewait: None,
            enew: None,
            mem_len: mem_len,
        }
    }

    /// Specifies a queue to use for this call only.
    pub fn queue(mut self, queue: &'b Queue) -> BufferCmd<'b, T> {
        self.queue = queue;
        self
    }

    /// Specifies whether or not to block thread until completion.
    ///
    /// Ignored if this is a copy, fill, or copy to image operation.
    ///
    /// ## Panics
    ///
    /// Will panic if `::read` has already been called. Use `::read_async`
    /// (unsafe) for a non-blocking read operation.
    ///
    pub fn block(mut self, block: bool) -> BufferCmd<'b, T> {
        if !block && self.lock_block {
            panic!("ocl::BufferCmd::block(): Blocking for this command has been disabled by \
                the '::read' method. For non-blocking reads use '::read_async'.");
        }
        self.block = block;
        self
    }

    /// Sets the linear offset for an operation.
    ///
    /// ## Panics
    ///
    /// The 'shape' may not have already been set to rectangular by the
    /// `::rect` function.
    pub fn offset(mut self, offset: usize)  -> BufferCmd<'b, T> {
        if let BufferCmdDataShape::Rect { .. } = self.shape {
            panic!("ocl::BufferCmd::offset(): This command builder has already been set to \
                rectangular mode with '::rect`. You cannot call both '::offset' and '::rect'.");
        }

        self.shape = BufferCmdDataShape::Lin { offset: offset };
        self
    }

    /// Specifies that this command will be a blocking read operation.
    ///
    /// After calling this method, the blocking state of this command will
    /// be locked to true and a call to `::block` will cause a panic.
    ///
    /// ## Panics
    ///
    /// The command operation kind must not have already been specified.
    ///
    pub fn read(mut self, dst_data: &'b mut [T]) -> BufferCmd<'b, T> {
        assert!(self.kind.is_unspec(), "ocl::BufferCmd::read(): Operation kind \
            already set for this command.");
        self.kind = BufferCmdKind::Read { data: dst_data };
        self.block = true;
        self.lock_block = true;
        self
    }

    /// Specifies that this command will be a non-blocking, asynchronous read
    /// operation.
    ///
    /// Sets the block mode to false automatically but it may still be freely
    /// toggled back. If set back to `true` this method call becomes equivalent
    /// to calling `::read`.
    ///
    /// ## Safety
    ///
    /// Caller must ensure that the container referred to by `dst_data` lives
    /// until the call completes.
    ///
    /// ## Panics
    ///
    /// The command operation kind must not have already been specified
    ///
    pub unsafe fn read_async(mut self, dst_data: &'b mut [T]) -> BufferCmd<'b, T> {
        assert!(self.kind.is_unspec(), "ocl::BufferCmd::read(): Operation kind \
            already set for this command.");
        self.kind = BufferCmdKind::Read { data: dst_data };
        self
    }

    /// Specifies that this command will be a write operation.
    ///
    /// ## Panics
    ///
    /// The command operation kind must not have already been specified
    ///
    pub fn write(mut self, src_data: &'b [T]) -> BufferCmd<'b, T> {
        assert!(self.kind.is_unspec(), "ocl::BufferCmd::write(): Operation kind \
            already set for this command.");
        self.kind = BufferCmdKind::Write { data: src_data };
        self
    }

    /// Specifies that this command will be a map operation.
    ///
    /// ## Panics
    ///
    /// The command operation kind must not have already been specified
    ///
    pub fn map(mut self, flags: Option<MapFlags>, len: Option<usize>) -> BufferCmd<'b, T> {
        assert!(self.kind.is_unspec(), "ocl::BufferCmd::write(): Operation kind \
            already set for this command.");
        self.kind = BufferCmdKind::Map{ flags: flags, len: len };
        self
    }


    /// Specifies that this command will be a copy operation.
    ///
    /// If `.block(..)` has been set it will be ignored.
    ///
    /// ## Errors
    ///
    /// If this is a rectangular copy, `dst_offset` and `len` must be zero.
    ///
    /// ## Panics
    ///
    /// The command operation kind must not have already been specified
    ///
    pub fn copy<M>(mut self, dst_buffer: &'b M, dst_offset: Option<usize>, len: Option<usize>)
            -> BufferCmd<'b, T>
            where M: AsMemRef<T>
    {
        assert!(self.kind.is_unspec(), "ocl::BufferCmd::copy(): Operation kind \
            already set for this command.");
        self.kind = BufferCmdKind::Copy {
            dst_buffer: dst_buffer.as_mem_ref(),
            dst_offset: dst_offset,
            len: len,
        };
        self
    }

    /// Specifies that this command will be a copy to image.
    ///
    /// If `.block(..)` has been set it will be ignored.
    ///
    ///
    /// ## Panics
    ///
    /// The command operation kind must not have already been specified
    ///
    pub fn copy_to_image(mut self, image: &'b MemCore, dst_origin: [usize; 3],
                region: [usize; 3]) -> BufferCmd<'b, T>
    {
        assert!(self.kind.is_unspec(), "ocl::BufferCmd::copy_to_image(): Operation kind \
            already set for this command.");
        self.kind = BufferCmdKind::CopyToImage { image: image, dst_origin: dst_origin, region: region };
        self
    }

    /// Specifies that this command will acquire a GL buffer.
    ///
    /// If `.block(..)` has been set it will be ignored.
    ///
    /// ## Panics
    ///
    /// The command operation kind must not have already been specified
    ///
    pub fn gl_acquire(mut self) -> BufferCmd<'b, T> {
        assert!(self.kind.is_unspec(), "ocl::BufferCmd::gl_acquire(): Operation kind \
            already set for this command.");
        self.kind = BufferCmdKind::GLAcquire;
        self
    }

    /// Specifies that this command will release a GL buffer.
    ///
    /// If `.block(..)` has been set it will be ignored.
    ///
    /// ## Panics
    ///
    /// The command operation kind must not have already been specified
    ///
    pub fn gl_release(mut self) -> BufferCmd<'b, T> {
        assert!(self.kind.is_unspec(), "ocl::BufferCmd::gl_release(): Operation kind \
            already set for this command.");
        self.kind = BufferCmdKind::GLRelease;
        self
    }

    /// Specifies that this command will be a fill.
    ///
    /// If `.block(..)` has been set it will be ignored.
    ///
    /// `pattern` is the vector or scalar value to repeat contiguously. `len`
    /// is the overall size expressed in units of sizeof(T) If `len` is `None`,
    /// the pattern will fill the entire buffer, otherwise, `len` must be
    /// divisible by sizeof(`pattern`).
    ///
    /// As an example if you want to fill the first 100 `cl_float4` sized
    /// elements of a buffer, `pattern` would be a `cl_float4` and `len` would
    /// be 400.
    ///
    /// ## Panics
    ///
    /// The command operation kind must not have already been specified
    ///
    pub fn fill(mut self, pattern: T, len: Option<usize>) -> BufferCmd<'b, T> {
        assert!(self.kind.is_unspec(), "ocl::BufferCmd::fill(): Operation kind \
            already set for this command.");
        self.kind = BufferCmdKind::Fill { pattern: pattern, len: len };
        self
    }

    /// Specifies that this will be a rectangularly shaped operation
    /// (the default being linear).
    ///
    /// Only valid for 'read', 'write', and 'copy' modes. Will error if used
    /// with 'fill' or 'copy to image'.
    pub fn rect(mut self, src_origin: [usize; 3], dst_origin: [usize; 3], region: [usize; 3],
                src_row_pitch: usize, src_slc_pitch: usize, dst_row_pitch: usize,
                dst_slc_pitch: usize) -> BufferCmd<'b, T>
    {
        if let BufferCmdDataShape::Lin { offset } = self.shape {
            assert!(offset == 0, "ocl::BufferCmd::rect(): This command builder has already been \
                set to linear mode with '::offset`. You cannot call both '::offset' and '::rect'.");
        }

        self.shape = BufferCmdDataShape::Rect { src_origin: src_origin, dst_origin: dst_origin,
            region: region, src_row_pitch: src_row_pitch, src_slc_pitch: src_slc_pitch,
            dst_row_pitch: dst_row_pitch, dst_slc_pitch: dst_slc_pitch };
        self
    }

    /// Specifies a list of events to wait on before the command will run.
    pub fn ewait(mut self, ewait: &'b ClWaitList) -> BufferCmd<'b, T> {
        self.ewait = Some(ewait);
        self
    }

    /// Specifies a list of events to wait on before the command will run or
    /// resets it to `None`.
    pub fn ewait_opt(mut self, ewait: Option<&'b ClWaitList>) -> BufferCmd<'b, T> {
        self.ewait = ewait;
        self
    }

    /// Specifies the destination for a new, optionally created event
    /// associated with this command.
    pub fn enew(mut self, enew: &'b mut ClEventPtrNew) -> BufferCmd<'b, T> {
        self.enew = Some(enew);
        self
    }

    /// Specifies a destination for a new, optionally created event
    /// associated with this command or resets it to `None`.
    pub fn enew_opt(mut self, enew: Option<&'b mut ClEventPtrNew>) -> BufferCmd<'b, T> {
        self.enew = enew;
        self
    }


    // core::enqueue_copy_buffer::<f32, core::EventList>(&queue, &src_buffer, &dst_buffer,
    //     copy_range.0, copy_range.0, copy_range.1 - copy_range.0, None,
    //     None).unwrap();

    /// Enqueues this command.
    ///
    /// For map operations use `::enq_map` instead.
    ///
    pub fn enq(self) -> OclResult<()> {
        match self.kind {
            BufferCmdKind::Read { data } => {
                match self.shape {
                    BufferCmdDataShape::Lin { offset } => {
                        try!(check_len(self.mem_len, data.len(), offset));

                        unsafe { core::enqueue_read_buffer(self.queue, self.obj_core, self.block,
                            offset, data, self.ewait, self.enew) }
                    },
                    BufferCmdDataShape::Rect { src_origin, dst_origin, region, src_row_pitch, src_slc_pitch,
                            dst_row_pitch, dst_slc_pitch } =>
                    {
                        // Verify dims given.
                        // try!(Ok(()));

                        unsafe { core::enqueue_read_buffer_rect(self.queue, self.obj_core,
                            self.block, src_origin, dst_origin, region, src_row_pitch,
                            src_slc_pitch, dst_row_pitch, dst_slc_pitch, data,
                            self.ewait, self.enew) }
                    }
                }
            },
            BufferCmdKind::Write { data } => {
                match self.shape {
                    BufferCmdDataShape::Lin { offset } => {
                        try!(check_len(self.mem_len, data.len(), offset));
                        core::enqueue_write_buffer(self.queue, self.obj_core, self.block,
                            offset, data, self.ewait, self.enew)
                    },
                    BufferCmdDataShape::Rect { src_origin, dst_origin, region, src_row_pitch, src_slc_pitch,
                            dst_row_pitch, dst_slc_pitch } =>
                    {
                        // Verify dims given.
                        // try!(Ok(()));

                        core::enqueue_write_buffer_rect(self.queue, self.obj_core,
                            self.block, src_origin, dst_origin, region, src_row_pitch,
                            src_slc_pitch, dst_row_pitch, dst_slc_pitch, data,
                            self.ewait, self.enew)
                    }
                }
            },
            BufferCmdKind::Copy { dst_buffer, dst_offset, len } => {
                match self.shape {
                    BufferCmdDataShape::Lin { offset } => {
                        let len = len.unwrap_or(self.mem_len);
                        try!(check_len(self.mem_len, len, offset));
                        let dst_offset = dst_offset.unwrap_or(0);

                        core::enqueue_copy_buffer::<T>(self.queue,
                            self.obj_core, dst_buffer, offset, dst_offset, len,
                            self.ewait, self.enew)
                    },
                    BufferCmdDataShape::Rect { src_origin, dst_origin, region, src_row_pitch, src_slc_pitch,
                            dst_row_pitch, dst_slc_pitch } =>
                    {
                        // Verify dims given.
                        // try!(Ok(()));

                        if dst_offset.is_some() || len.is_some() { return OclError::err(
                            "ocl::BufferCmd::enq(): For 'rect' shaped copies, destination \
                            offset and length must be 'None'. Ex.: \
                            'cmd().copy(&{{buf_name}}, None, None)..'.");
                        }
                        core::enqueue_copy_buffer_rect::<T>(self.queue, self.obj_core, dst_buffer,
                            src_origin, dst_origin, region, src_row_pitch, src_slc_pitch,
                            dst_row_pitch, dst_slc_pitch, self.ewait, self.enew)
                    },
                }
            },
            BufferCmdKind::Fill { pattern, len } => {
                match self.shape {
                    BufferCmdDataShape::Lin { offset } => {
                        let len = match len {
                            Some(l) => l,
                            None => self.mem_len,
                        };
                        try!(check_len(self.mem_len, len, offset));
                        core::enqueue_fill_buffer(self.queue, self.obj_core, pattern,
                            offset, len, self.ewait, self.enew, Some(&self.queue.device_version()))
                    },
                    BufferCmdDataShape::Rect { .. } => OclError::err("ocl::BufferCmd::enq(): \
                        Rectangular fill is not a valid operation. Please use the default shape, linear.")
                }
            },
            BufferCmdKind::GLAcquire => {
                core::enqueue_acquire_gl_buffer(self.queue, self.obj_core, self.ewait, self.enew)
            },
            BufferCmdKind::GLRelease => {
                core::enqueue_release_gl_buffer(self.queue, self.obj_core, self.ewait, self.enew)
            },
            BufferCmdKind::Unspecified => OclError::err("ocl::BufferCmd::enq(): No operation \
                specified. Use '.read(...)', 'write(...)', etc. before calling '.enq()'."),
            BufferCmdKind::Map { .. } => OclError::err("ocl::BufferCmd::enq(): \
                For map operations use '::enq_map()' instead."),
            _ => unimplemented!(),
        }
    }

    /// Enqueues a map command.
    ///
    /// For all other operation types use `::map` instead.
    ///
    pub fn enq_map(self) -> OclResult<MappedMem<T>> {
        match self.kind {
            BufferCmdKind::Map { flags, len } => {
                match self.shape {
                    BufferCmdDataShape::Lin { offset } => {
                        let len = match len {
                            Some(l) => l,
                            None => self.mem_len,
                        };

                        check_len(self.mem_len, len, offset)?;
                        let flags = flags.unwrap_or(MapFlags::empty());

                        unsafe { Ok(MappedMem::new(core::enqueue_map_buffer::<T>(self.queue,
                            self.obj_core, self.block, flags, offset, len, self.ewait,
                            self.enew )?)) }
                    },
                    BufferCmdDataShape::Rect { .. } => {
                        OclError::err("ocl::BufferCmd::enq_map(): A rectangular map is not a valid \
                            operation. Please use the default shape, linear.")
                    },
                }
            },
            BufferCmdKind::Unspecified => OclError::err("ocl::BufferCmd::enq_map(): No operation \
                specified. Use '::map', before calling '::enq_map'."),
            _ => OclError::err("ocl::BufferCmd::enq_map(): For non-map operations use '::enq' instead."),
        }
    }
}


/// A chunk of memory physically located on a device, such as a GPU.
///
/// Data is stored remotely in a memory buffer on the device associated with
/// `queue`.
///
#[derive(Debug, Clone)]
pub struct Buffer<T: OclPrm> {
    obj_core: MemCore,
    queue: Queue,
    dims: SpatialDims,
    len: usize,
    flags: MemFlags,
    _data: PhantomData<T>,
}

impl<T: OclPrm> Buffer<T> {
    /// Creates a new sub-buffer.
    ///
    /// `flags` defaults to `flags::MEM_READ_WRITE` if `None` is passed. See
    /// the [SDK Docs] for more information about flags. Note that the
    /// `host_ptr` mentioned in the [SDK Docs] is equivalent to the slice
    /// optionally passed as the `data` argument. Also note that the names of
    /// the flags in this library have the `CL_` prefix removed for brevity.
    ///
    /// [SDK Docs]: https://www.khronos.org/registry/cl/sdk/1.2/docs/man/xhtml/clCreateBuffer.html
    ///
    ///
    /// [UNSTABLE]: Arguments may still be in a state of flux.
    ///
    pub fn new<D: Into<SpatialDims>>(queue: Queue, flags_opt: Option<MemFlags>, dims: D,
                data: Option<&[T]>) -> OclResult<Buffer<T>> {
        let flags = flags_opt.unwrap_or(::flags::MEM_READ_WRITE);
        let dims: SpatialDims = dims.into();
        let len = dims.to_len();
        let obj_core = unsafe { try!(core::create_buffer(queue.context_core_as_ref(), flags, len,
            data)) };

        let buf = Buffer {
            obj_core: obj_core,
            queue: queue,
            dims: dims,
            len: len,
            flags: flags,
            _data: PhantomData,
        };

        if data.is_none() {
            // Useful on platforms (PoCL) that have trouble with fill. Creates
            // a temporary zeroed `Vec` in host memory and writes from there
            // instead. Add `features = ["buffer_no_fill"]` to your Cargo.toml.
            if cfg!(feature = "buffer_no_fill") {
                // println!("#### no fill");
                try!(buf.cmd().fill(Default::default(), None).enq());
            } else {
                let zeros = vec![Default::default(); len];
                try!(buf.cmd().write(&zeros).enq());
                // println!("#### fill!");
            }
        }

        Ok(buf)
    }

    /// [UNTESTED]
    /// Creates a buffer linked to a previously created OpenGL buffer object.
    ///
    ///
    /// ### Errors
    ///
    /// Don't forget to `.cmd().gl_acquire().enq()` before using it and
    /// `.cmd().gl_release().enq()` after.
    ///
    /// See the [`BufferCmd` docs](/ocl/ocl/build/struct.BufferCmd.html)
    /// for more info.
    ///
    pub fn from_gl_buffer<D: MemLen>(queue: Queue, flags_opt: Option<MemFlags>, dims: D,
            gl_object: cl_GLuint) -> OclResult<Buffer<T>> {
        let flags = flags_opt.unwrap_or(core::MEM_READ_WRITE);
        let dims: SpatialDims = dims.to_lens().into();
        let len = dims.to_len();
        let obj_core = unsafe { try!(core::create_from_gl_buffer(
            queue.context_core_as_ref(),
            gl_object,
            flags))
        };

        let buf = Buffer {
            obj_core: obj_core,
            queue: queue,
            dims: dims,
            len: len,
            _data: PhantomData,
            flags: flags,
        };

        Ok(buf)
    }

    /// Returns a buffer command builder used to read, write, copy, etc.
    ///
    /// Call `.enq()` to enqueue the command.
    ///
    /// See the [`BufferCmd` docs](/ocl/ocl/build/struct.BufferCmd.html)
    /// for more info.
    ///
    #[inline]
    pub fn cmd(&self) -> BufferCmd<T> {
        BufferCmd::new(&self.queue, &self.obj_core, self.len)
    }

    /// Returns a buffer command builder used to read.
    ///
    /// Call `.enq()` to enqueue the command.
    ///
    /// See the [`BufferCmd` docs](/ocl/ocl/build/struct.BufferCmd.html)
    /// for more info.
    ///
    #[inline]
    pub fn read<'b>(&'b self, data: &'b mut [T]) -> BufferCmd<'b, T> {
        self.cmd().read(data)
    }

    /// Returns a buffer command builder used to write.
    ///
    /// Call `.enq()` to enqueue the command.
    ///
    /// See the [`BufferCmd` docs](/ocl/ocl/build/struct.BufferCmd.html)
    /// for more info.
    ///
    #[inline]
    pub fn write<'b>(&'b self, data: &'b [T]) -> BufferCmd<'b, T> {
        self.cmd().write(data)
    }

    /// Returns the length of the buffer.
    #[inline]
    pub fn len(&self) -> usize {
        // debug_assert!((if let VecOption::Some(ref vec) = self.vec { vec.len() }
        //     else { self.len }) == self.len);
        self.len
    }

    /// Returns the dimensions of the buffer.
    #[inline]
    pub fn dims(&self) -> &SpatialDims {
        &self.dims
    }

    /// Returns info about the underlying memory object.
    #[inline]
    pub fn mem_info(&self, info_kind: MemInfo) -> MemInfoResult {
        // match core::get_mem_object_info(&self.obj_core, info_kind) {
        //     Ok(res) => res,
        //     Err(err) => MemInfoResult::Error(Box::new(err)),
        // }
        core::get_mem_object_info(&self.obj_core, info_kind)
    }

    /// Changes the default queue used by this Buffer for reads and writes, etc.
    ///
    /// Returns a mutable reference for optional chaining i.e.:
    ///
    /// ### Example
    ///
    /// `buffer.set_default_queue(queue).read(....);`
    ///
    #[inline]
    pub fn set_default_queue<'a>(&'a mut self, queue: &Queue) -> &'a mut Buffer<T> {
        // [FIXME]: Set this up:
        // assert!(queue.device == self.queue.device);
        // [/FIXME]
        self.queue = queue.clone();
        self
    }

    /// Returns a reference to the default queue.
    ///
    #[inline]
    pub fn default_queue(&self) -> &Queue {
        &self.queue
    }

    /// Returns a reference to the core pointer wrapper, usable by functions in
    /// the `core` module.
    ///
    #[inline]
    pub fn core_as_ref(&self) -> &MemCore {
        &self.obj_core
    }

    /// Returns the memory flags used during the creation of this buffer.
    ///
    /// Saves the cost of having to look them up using `::mem_info`.
    ///
    #[inline]
    pub fn flags(&self) -> MemFlags {
        self.flags
    }

    /// Creates a new sub-buffer and returns it if successful.
    ///
    /// `flags` defaults to `flags::MEM_READ_WRITE` if `None` is passed. See
    /// the [SDK Docs] for more information about flags. Note that the names
    /// of the flags in this library have the `CL_` prefix removed for
    /// brevity.
    ///
    /// `origin` and `size` set up the region of the sub-buffer within the
    ///  original buffer and must not fall beyond the boundaries of it.
    ///
    /// `origin` must be a multiple of the `DeviceInfo::MemBaseAddrAlign`
    /// otherwise you will get a `CL_MISALIGNED_SUB_BUFFER_OFFSET` error. To
    /// determine, use `Device::mem_base_addr_align` for the device associated
    /// with the queue which will be use with this sub-buffer.
    ///
    /// [SDK Docs]: https://www.khronos.org/registry/cl/sdk/1.2/docs/man/xhtml/clCreateSubBuffer.html
    ///
    #[inline]
    pub fn create_sub_buffer<D: Into<SpatialDims>>(&self, flags: Option<MemFlags>, origin: D,
        size: D) -> OclResult<SubBuffer<T>>
    {
        SubBuffer::new(self, flags, origin, size)
    }

    /// Formats memory info.
    #[inline]
    fn fmt_mem_info(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Buffer Mem")
            .field("Type", &self.mem_info(MemInfo::Type))
            .field("Flags", &self.mem_info(MemInfo::Flags))
            .field("Size", &self.mem_info(MemInfo::Size))
            .field("HostPtr", &self.mem_info(MemInfo::HostPtr))
            .field("MapCount", &self.mem_info(MemInfo::MapCount))
            .field("ReferenceCount", &self.mem_info(MemInfo::ReferenceCount))
            .field("Context", &self.mem_info(MemInfo::Context))
            .field("AssociatedMemobject", &self.mem_info(MemInfo::AssociatedMemobject))
            .field("Offset", &self.mem_info(MemInfo::Offset))
            .finish()
    }
}

impl<T: OclPrm> Deref for Buffer<T> {
    type Target = MemCore;

    fn deref(&self) -> &MemCore {
        &self.obj_core
    }
}

impl<T: OclPrm> DerefMut for Buffer<T> {
    fn deref_mut(&mut self) -> &mut MemCore {
        &mut self.obj_core
    }
}

impl<T: OclPrm> AsRef<MemCore> for Buffer<T> {
    fn as_ref(&self) -> &MemCore {
        &self.obj_core
    }
}

impl<T: OclPrm> AsMut<MemCore> for Buffer<T> {
    fn as_mut(&mut self) -> &mut MemCore {
        &mut self.obj_core
    }
}

impl<T: OclPrm> AsMemRef<T> for Buffer<T> {
    fn as_mem_ref(&self) -> &MemCore {
        &self.obj_core
    }
}

impl<'a, T: OclPrm> AsMemRef<T> for &'a Buffer<T> {
    fn as_mem_ref(&self) -> &MemCore {
        &self.obj_core
    }
}

impl<'a, T: OclPrm> AsMemRef<T> for &'a mut Buffer<T> {
    fn as_mem_ref(&self) -> &MemCore {
        &self.obj_core
    }
}

// impl<T: OclPrm> AsMemRef<T> for Buffer<T> {
//     fn as_mem_ref(&mut self) -> &mut MemCore {
//         &self.obj_core
//     }
// }

// impl<'a, T: OclPrm> AsMemRef<T> for &'a Buffer<T> {
//     fn as_mem_ref(&self) -> &mut MemCore {
//         &self.obj_core
//     }
// }

impl<T: OclPrm> std::fmt::Display for Buffer<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.fmt_mem_info(f)
    }
}


/// A subsection of buffer memory physically located on a device, such as a
/// GPU.
///
#[derive(Debug, Clone)]
pub struct SubBuffer<T: OclPrm> {
    obj_core: MemCore,
    queue: Queue,
    origin: SpatialDims,
    size: SpatialDims,
    len: usize,
    flags: MemFlags,
    _data: PhantomData<T>,
}

impl<T: OclPrm> SubBuffer<T> {
    /// Creates a new sub-buffer.
    ///
    /// `flags` defaults to `flags::MEM_READ_WRITE` if `None` is passed. See
    /// the [SDK Docs] for more information about flags. Note that the names
    /// of the flags in this library have the `CL_` prefix removed for
    /// brevity.
    ///
    /// `origin` and `size` set up the region of the sub-buffer within the
    ///  original buffer and must not fall beyond the boundaries of it.
    ///
    /// `origin` must be a multiple of the `DeviceInfo::MemBaseAddrAlign`
    /// otherwise you will get a `CL_MISALIGNED_SUB_BUFFER_OFFSET` error. To
    /// determine, use `Device::mem_base_addr_align` for the device associated
    /// with the queue which will be use with this sub-buffer.
    ///
    /// [SDK Docs]: https://www.khronos.org/registry/cl/sdk/1.2/docs/man/xhtml/clCreateSubBuffer.html
    ///
    pub fn new<D: Into<SpatialDims>>(buffer: &Buffer<T>, flags_opt: Option<MemFlags>, origin: D,
        size: D) -> OclResult<SubBuffer<T>>
    {
        let flags = flags_opt.unwrap_or(::flags::MEM_READ_WRITE);
        let origin: SpatialDims = origin.into();
        let size: SpatialDims = size.into();

        let buffer_len = buffer.dims().to_len();
        let origin_len = origin.to_len();
        let size_len = size.to_len();

        if origin_len > buffer_len {
            return OclError::err(format!("SubBuffer::new: Origin ({:?}) is outside of the \
                dimensions of the source buffer ({:?}).", origin, buffer.dims()));
        }

        if origin_len + size_len > buffer_len {
            return OclError::err(format!("SubBuffer::new: Sub-buffer region (origin: '{:?}', \
                size: '{:?}') exceeds the dimensions of the source buffer ({:?}).", origin, size,
                buffer.dims()));
        }

        let obj_core = core::create_sub_buffer::<T>(buffer, flags,
            &BufferRegion::new(origin.to_len(), size.to_len()))?;

        Ok(SubBuffer {
            obj_core: obj_core,
            queue: buffer.default_queue().clone(),
            origin: origin,
            size: size,
            len: size_len,
            flags: flags,
            _data: PhantomData,
        })
    }

    /// Returns a buffer command builder used to read, write, copy, etc.
    ///
    /// Call `.enq()` to enqueue the command.
    ///
    /// See the [`BufferCmd` docs](/ocl/ocl/build/struct.BufferCmd.html)
    /// for more info.
    ///
    #[inline]
    pub fn cmd(&self) -> BufferCmd<T> {
        BufferCmd::new(&self.queue, &self.obj_core, self.len)
    }

    /// Returns a buffer command builder used to read.
    ///
    /// Call `.enq()` to enqueue the command.
    ///
    /// See the [`BufferCmd` docs](/ocl/ocl/build/struct.BufferCmd.html)
    /// for more info.
    ///
    #[inline]
    pub fn read<'b>(&'b self, data: &'b mut [T]) -> BufferCmd<'b, T> {
        self.cmd().read(data)
    }

    /// Returns a buffer command builder used to write.
    ///
    /// Call `.enq()` to enqueue the command.
    ///
    /// See the [`BufferCmd` docs](/ocl/ocl/build/struct.BufferCmd.html)
    /// for more info.
    ///
    #[inline]
    pub fn write<'b>(&'b self, data: &'b [T]) -> BufferCmd<'b, T> {
        self.cmd().write(data)
    }

    /// Returns the origin of the sub-buffer within the buffer.
    #[inline]
    pub fn origin(&self) -> &SpatialDims {
        &self.origin
    }

    /// Returns the dimensions of the sub-buffer.
    #[inline]
    pub fn dims(&self) -> &SpatialDims {
        &self.size
    }

    /// Returns the length of the sub-buffer.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns info about the underlying memory object.
    #[inline]
    pub fn mem_info(&self, info_kind: MemInfo) -> MemInfoResult {
        core::get_mem_object_info(&self.obj_core, info_kind)
    }

    /// Changes the default queue used by this `SubBuffer` for reads and
    /// writes, etc.
    ///
    /// Returns a mutable reference for optional chaining i.e.:
    ///
    /// ### Example
    ///
    /// `buffer.set_default_queue(queue).read(....);`
    ///
    #[inline]
    pub fn set_default_queue<'a>(&'a mut self, queue: Queue) -> &'a mut SubBuffer<T> {
        // [FIXME]: Set this up:
        // assert!(queue.device == self.queue.device);
        // [/FIXME]
        self.queue = queue;
        self
    }

    /// Returns a reference to the default queue.
    #[inline]
    pub fn default_queue(&self) -> &Queue {
        &self.queue
    }

    /// Returns a reference to the core pointer wrapper, usable by functions in
    /// the `core` module.
    #[inline]
    pub fn core_as_ref(&self) -> &MemCore {
        &self.obj_core
    }

    /// Returns the memory flags used during the creation of this sub-buffer.
    ///
    /// Saves the cost of having to look them up using `::mem_info`.
    ///
    #[inline]
    pub fn flags(&self) -> MemFlags {
        self.flags
    }

    /// Formats memory info.
    #[inline]
    fn fmt_mem_info(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("SubBuffer Mem")
            .field("Type", &self.mem_info(MemInfo::Type))
            .field("Flags", &self.mem_info(MemInfo::Flags))
            .field("Size", &self.mem_info(MemInfo::Size))
            .field("HostPtr", &self.mem_info(MemInfo::HostPtr))
            .field("MapCount", &self.mem_info(MemInfo::MapCount))
            .field("ReferenceCount", &self.mem_info(MemInfo::ReferenceCount))
            .field("Context", &self.mem_info(MemInfo::Context))
            .field("AssociatedMemobject", &self.mem_info(MemInfo::AssociatedMemobject))
            .field("Offset", &self.mem_info(MemInfo::Offset))
            .finish()
    }
}

impl<T: OclPrm> Deref for SubBuffer<T> {
    type Target = MemCore;

    fn deref(&self) -> &MemCore {
        &self.obj_core
    }
}

impl<T: OclPrm> DerefMut for SubBuffer<T> {
    fn deref_mut(&mut self) -> &mut MemCore {
        &mut self.obj_core
    }
}

impl<T: OclPrm> AsRef<MemCore> for SubBuffer<T> {
    fn as_ref(&self) -> &MemCore {
        &self.obj_core
    }
}

impl<T: OclPrm> AsMut<MemCore> for SubBuffer<T> {
    fn as_mut(&mut self) -> &mut MemCore {
        &mut self.obj_core
    }
}

impl<'a, T: OclPrm> AsMemRef<T> for SubBuffer<T> {
    fn as_mem_ref(&self) -> &MemCore {
        &self.obj_core
    }
}

impl<'a, T: OclPrm> AsMemRef<T> for &'a SubBuffer<T> {
    fn as_mem_ref(&self) -> &MemCore {
        &self.obj_core
    }
}

impl<'a, T: OclPrm> AsMemRef<T> for &'a mut SubBuffer<T> {
    fn as_mem_ref(&self) -> &MemCore {
        &self.obj_core
    }
}

impl<T: OclPrm> std::fmt::Display for SubBuffer<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.fmt_mem_info(f)
    }
}
