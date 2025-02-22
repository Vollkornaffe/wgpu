use hal::CommandEncoder as _;

#[cfg(feature = "trace")]
use crate::device::trace::Command as TraceCommand;
use crate::{
    command::{CommandBuffer, CommandEncoderError},
    device::{DeviceError, MissingFeatures},
    global::Global,
    hal_api::HalApi,
    id::{self, Id},
    init_tracker::MemoryInitKind,
    resource::{ParentDevice, QuerySet},
    storage::Storage,
    Epoch, FastHashMap, Index,
};
use std::{iter, marker::PhantomData, sync::Arc};
use thiserror::Error;
use wgt::BufferAddress;

#[derive(Debug)]
pub(crate) struct QueryResetMap<A: HalApi> {
    map: FastHashMap<Index, (Vec<bool>, Epoch)>,
    _phantom: PhantomData<A>,
}
impl<A: HalApi> QueryResetMap<A> {
    pub fn new() -> Self {
        Self {
            map: FastHashMap::default(),
            _phantom: PhantomData,
        }
    }

    pub fn use_query_set(
        &mut self,
        id: id::QuerySetId,
        query_set: &QuerySet<A>,
        query: u32,
    ) -> bool {
        let (index, epoch, _) = id.unzip();
        let vec_pair = self
            .map
            .entry(index)
            .or_insert_with(|| (vec![false; query_set.desc.count as usize], epoch));

        std::mem::replace(&mut vec_pair.0[query as usize], true)
    }

    pub fn reset_queries(
        &mut self,
        raw_encoder: &mut A::CommandEncoder,
        query_set_storage: &Storage<QuerySet<A>>,
        backend: wgt::Backend,
    ) -> Result<(), id::QuerySetId> {
        for (query_set_id, (state, epoch)) in self.map.drain() {
            let id = Id::zip(query_set_id, epoch, backend);
            let query_set = query_set_storage.get(id).map_err(|_| id)?;

            debug_assert_eq!(state.len(), query_set.desc.count as usize);

            // Need to find all "runs" of values which need resets. If the state vector is:
            // [false, true, true, false, true], we want to reset [1..3, 4..5]. This minimizes
            // the amount of resets needed.
            let mut run_start: Option<u32> = None;
            for (idx, value) in state.into_iter().chain(iter::once(false)).enumerate() {
                match (run_start, value) {
                    // We're inside of a run, do nothing
                    (Some(..), true) => {}
                    // We've hit the end of a run, dispatch a reset
                    (Some(start), false) => {
                        run_start = None;
                        unsafe { raw_encoder.reset_queries(query_set.raw(), start..idx as u32) };
                    }
                    // We're starting a run
                    (None, true) => {
                        run_start = Some(idx as u32);
                    }
                    // We're in a run of falses, do nothing.
                    (None, false) => {}
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SimplifiedQueryType {
    Occlusion,
    Timestamp,
    PipelineStatistics,
}
impl From<wgt::QueryType> for SimplifiedQueryType {
    fn from(q: wgt::QueryType) -> Self {
        match q {
            wgt::QueryType::Occlusion => SimplifiedQueryType::Occlusion,
            wgt::QueryType::Timestamp => SimplifiedQueryType::Timestamp,
            wgt::QueryType::PipelineStatistics(..) => SimplifiedQueryType::PipelineStatistics,
        }
    }
}

/// Error encountered when dealing with queries
#[derive(Clone, Debug, Error)]
#[non_exhaustive]
pub enum QueryError {
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error(transparent)]
    Encoder(#[from] CommandEncoderError),
    #[error(transparent)]
    MissingFeature(#[from] MissingFeatures),
    #[error("Error encountered while trying to use queries")]
    Use(#[from] QueryUseError),
    #[error("Error encountered while trying to resolve a query")]
    Resolve(#[from] ResolveError),
    #[error("Buffer {0:?} is invalid or destroyed")]
    InvalidBuffer(id::BufferId),
    #[error("QuerySet {0:?} is invalid or destroyed")]
    InvalidQuerySet(id::QuerySetId),
}

impl crate::error::PrettyError for QueryError {
    fn fmt_pretty(&self, fmt: &mut crate::error::ErrorFormatter) {
        fmt.error(self);
        match *self {
            Self::InvalidBuffer(id) => fmt.buffer_label(&id),
            Self::InvalidQuerySet(id) => fmt.query_set_label(&id),

            _ => {}
        }
    }
}

/// Error encountered while trying to use queries
#[derive(Clone, Debug, Error)]
#[non_exhaustive]
pub enum QueryUseError {
    #[error("Query {query_index} is out of bounds for a query set of size {query_set_size}")]
    OutOfBounds {
        query_index: u32,
        query_set_size: u32,
    },
    #[error("Query {query_index} has already been used within the same renderpass. Queries must only be used once per renderpass")]
    UsedTwiceInsideRenderpass { query_index: u32 },
    #[error("Query {new_query_index} was started while query {active_query_index} was already active. No more than one statistic or occlusion query may be active at once")]
    AlreadyStarted {
        active_query_index: u32,
        new_query_index: u32,
    },
    #[error("Query was stopped while there was no active query")]
    AlreadyStopped,
    #[error("A query of type {query_type:?} was started using a query set of type {set_type:?}")]
    IncompatibleType {
        set_type: SimplifiedQueryType,
        query_type: SimplifiedQueryType,
    },
}

/// Error encountered while trying to resolve a query.
#[derive(Clone, Debug, Error)]
#[non_exhaustive]
pub enum ResolveError {
    #[error("Queries can only be resolved to buffers that contain the QUERY_RESOLVE usage")]
    MissingBufferUsage,
    #[error("Resolve buffer offset has to be aligned to `QUERY_RESOLVE_BUFFER_ALIGNMENT")]
    BufferOffsetAlignment,
    #[error("Resolving queries {start_query}..{end_query} would overrun the query set of size {query_set_size}")]
    QueryOverrun {
        start_query: u32,
        end_query: u32,
        query_set_size: u32,
    },
    #[error("Resolving queries {start_query}..{end_query} ({stride} byte queries) will end up overrunning the bounds of the destination buffer of size {buffer_size} using offsets {buffer_start_offset}..{buffer_end_offset}")]
    BufferOverrun {
        start_query: u32,
        end_query: u32,
        stride: u32,
        buffer_size: BufferAddress,
        buffer_start_offset: BufferAddress,
        buffer_end_offset: BufferAddress,
    },
}

impl<A: HalApi> QuerySet<A> {
    fn validate_query(
        &self,
        query_type: SimplifiedQueryType,
        query_index: u32,
        reset_state: Option<&mut QueryResetMap<A>>,
    ) -> Result<(), QueryUseError> {
        // We need to defer our resets because we are in a renderpass,
        // add the usage to the reset map.
        if let Some(reset) = reset_state {
            let used = reset.use_query_set(self.info.id(), self, query_index);
            if used {
                return Err(QueryUseError::UsedTwiceInsideRenderpass { query_index });
            }
        }

        let simple_set_type = SimplifiedQueryType::from(self.desc.ty);
        if simple_set_type != query_type {
            return Err(QueryUseError::IncompatibleType {
                query_type,
                set_type: simple_set_type,
            });
        }

        if query_index >= self.desc.count {
            return Err(QueryUseError::OutOfBounds {
                query_index,
                query_set_size: self.desc.count,
            });
        }

        Ok(())
    }

    pub(super) fn validate_and_write_timestamp(
        &self,
        raw_encoder: &mut A::CommandEncoder,
        query_index: u32,
        reset_state: Option<&mut QueryResetMap<A>>,
    ) -> Result<(), QueryUseError> {
        let needs_reset = reset_state.is_none();
        self.validate_query(SimplifiedQueryType::Timestamp, query_index, reset_state)?;

        unsafe {
            // If we don't have a reset state tracker which can defer resets, we must reset now.
            if needs_reset {
                raw_encoder.reset_queries(self.raw(), query_index..(query_index + 1));
            }
            raw_encoder.write_timestamp(self.raw(), query_index);
        }

        Ok(())
    }
}

pub(super) fn validate_and_begin_occlusion_query<A: HalApi>(
    query_set: Arc<QuerySet<A>>,
    raw_encoder: &mut A::CommandEncoder,
    query_index: u32,
    reset_state: Option<&mut QueryResetMap<A>>,
    active_query: &mut Option<(Arc<QuerySet<A>>, u32)>,
) -> Result<(), QueryUseError> {
    let needs_reset = reset_state.is_none();
    query_set.validate_query(SimplifiedQueryType::Occlusion, query_index, reset_state)?;

    if let Some((_old, old_idx)) = active_query.take() {
        return Err(QueryUseError::AlreadyStarted {
            active_query_index: old_idx,
            new_query_index: query_index,
        });
    }
    let (query_set, _) = &active_query.insert((query_set, query_index));

    unsafe {
        // If we don't have a reset state tracker which can defer resets, we must reset now.
        if needs_reset {
            raw_encoder.reset_queries(query_set.raw(), query_index..(query_index + 1));
        }
        raw_encoder.begin_query(query_set.raw(), query_index);
    }

    Ok(())
}

pub(super) fn end_occlusion_query<A: HalApi>(
    raw_encoder: &mut A::CommandEncoder,
    active_query: &mut Option<(Arc<QuerySet<A>>, u32)>,
) -> Result<(), QueryUseError> {
    if let Some((query_set, query_index)) = active_query.take() {
        unsafe { raw_encoder.end_query(query_set.raw.as_ref().unwrap(), query_index) };
        Ok(())
    } else {
        Err(QueryUseError::AlreadyStopped)
    }
}

pub(super) fn validate_and_begin_pipeline_statistics_query<A: HalApi>(
    query_set: Arc<QuerySet<A>>,
    raw_encoder: &mut A::CommandEncoder,
    query_index: u32,
    reset_state: Option<&mut QueryResetMap<A>>,
    active_query: &mut Option<(Arc<QuerySet<A>>, u32)>,
) -> Result<(), QueryUseError> {
    let needs_reset = reset_state.is_none();
    query_set.validate_query(
        SimplifiedQueryType::PipelineStatistics,
        query_index,
        reset_state,
    )?;

    if let Some((_old, old_idx)) = active_query.take() {
        return Err(QueryUseError::AlreadyStarted {
            active_query_index: old_idx,
            new_query_index: query_index,
        });
    }
    let (query_set, _) = &active_query.insert((query_set, query_index));

    unsafe {
        // If we don't have a reset state tracker which can defer resets, we must reset now.
        if needs_reset {
            raw_encoder.reset_queries(query_set.raw(), query_index..(query_index + 1));
        }
        raw_encoder.begin_query(query_set.raw(), query_index);
    }

    Ok(())
}

pub(super) fn end_pipeline_statistics_query<A: HalApi>(
    raw_encoder: &mut A::CommandEncoder,
    active_query: &mut Option<(Arc<QuerySet<A>>, u32)>,
) -> Result<(), QueryUseError> {
    if let Some((query_set, query_index)) = active_query.take() {
        unsafe { raw_encoder.end_query(query_set.raw(), query_index) };
        Ok(())
    } else {
        Err(QueryUseError::AlreadyStopped)
    }
}

impl Global {
    pub fn command_encoder_write_timestamp<A: HalApi>(
        &self,
        command_encoder_id: id::CommandEncoderId,
        query_set_id: id::QuerySetId,
        query_index: u32,
    ) -> Result<(), QueryError> {
        let hub = A::hub(self);

        let cmd_buf = CommandBuffer::get_encoder(hub, command_encoder_id)?;

        cmd_buf
            .device
            .require_features(wgt::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS)?;

        let mut cmd_buf_data = cmd_buf.data.lock();
        let cmd_buf_data = cmd_buf_data.as_mut().unwrap();

        #[cfg(feature = "trace")]
        if let Some(ref mut list) = cmd_buf_data.commands {
            list.push(TraceCommand::WriteTimestamp {
                query_set_id,
                query_index,
            });
        }

        let encoder = &mut cmd_buf_data.encoder;
        let tracker = &mut cmd_buf_data.trackers;

        let raw_encoder = encoder.open()?;

        let query_set_guard = hub.query_sets.read();
        let query_set = tracker
            .query_sets
            .add_single(&*query_set_guard, query_set_id)
            .ok_or(QueryError::InvalidQuerySet(query_set_id))?;

        query_set.validate_and_write_timestamp(raw_encoder, query_index, None)?;

        Ok(())
    }

    pub fn command_encoder_resolve_query_set<A: HalApi>(
        &self,
        command_encoder_id: id::CommandEncoderId,
        query_set_id: id::QuerySetId,
        start_query: u32,
        query_count: u32,
        destination: id::BufferId,
        destination_offset: BufferAddress,
    ) -> Result<(), QueryError> {
        let hub = A::hub(self);

        let cmd_buf = CommandBuffer::get_encoder(hub, command_encoder_id)?;
        let mut cmd_buf_data = cmd_buf.data.lock();
        let cmd_buf_data = cmd_buf_data.as_mut().unwrap();

        #[cfg(feature = "trace")]
        if let Some(ref mut list) = cmd_buf_data.commands {
            list.push(TraceCommand::ResolveQuerySet {
                query_set_id,
                start_query,
                query_count,
                destination,
                destination_offset,
            });
        }

        let encoder = &mut cmd_buf_data.encoder;
        let tracker = &mut cmd_buf_data.trackers;
        let buffer_memory_init_actions = &mut cmd_buf_data.buffer_memory_init_actions;
        let raw_encoder = encoder.open()?;

        if destination_offset % wgt::QUERY_RESOLVE_BUFFER_ALIGNMENT != 0 {
            return Err(QueryError::Resolve(ResolveError::BufferOffsetAlignment));
        }
        let query_set_guard = hub.query_sets.read();
        let query_set = tracker
            .query_sets
            .add_single(&*query_set_guard, query_set_id)
            .ok_or(QueryError::InvalidQuerySet(query_set_id))?;

        query_set.same_device_as(cmd_buf.as_ref())?;

        let (dst_buffer, dst_pending) = {
            let buffer_guard = hub.buffers.read();
            let dst_buffer = buffer_guard
                .get(destination)
                .map_err(|_| QueryError::InvalidBuffer(destination))?;

            dst_buffer.same_device_as(cmd_buf.as_ref())?;

            tracker
                .buffers
                .set_single(dst_buffer, hal::BufferUses::COPY_DST)
                .ok_or(QueryError::InvalidBuffer(destination))?
        };

        let snatch_guard = dst_buffer.device.snatchable_lock.read();

        let dst_barrier = dst_pending.map(|pending| pending.into_hal(&dst_buffer, &snatch_guard));

        if !dst_buffer.usage.contains(wgt::BufferUsages::QUERY_RESOLVE) {
            return Err(ResolveError::MissingBufferUsage.into());
        }

        let end_query = start_query + query_count;
        if end_query > query_set.desc.count {
            return Err(ResolveError::QueryOverrun {
                start_query,
                end_query,
                query_set_size: query_set.desc.count,
            }
            .into());
        }

        let elements_per_query = match query_set.desc.ty {
            wgt::QueryType::Occlusion => 1,
            wgt::QueryType::PipelineStatistics(ps) => ps.bits().count_ones(),
            wgt::QueryType::Timestamp => 1,
        };
        let stride = elements_per_query * wgt::QUERY_SIZE;
        let bytes_used = (stride * query_count) as BufferAddress;

        let buffer_start_offset = destination_offset;
        let buffer_end_offset = buffer_start_offset + bytes_used;

        if buffer_end_offset > dst_buffer.size {
            return Err(ResolveError::BufferOverrun {
                start_query,
                end_query,
                stride,
                buffer_size: dst_buffer.size,
                buffer_start_offset,
                buffer_end_offset,
            }
            .into());
        }

        // TODO(https://github.com/gfx-rs/wgpu/issues/3993): Need to track initialization state.
        buffer_memory_init_actions.extend(dst_buffer.initialization_status.read().create_action(
            &dst_buffer,
            buffer_start_offset..buffer_end_offset,
            MemoryInitKind::ImplicitlyInitialized,
        ));

        let raw_dst_buffer = dst_buffer
            .raw(&snatch_guard)
            .ok_or(QueryError::InvalidBuffer(destination))?;

        unsafe {
            raw_encoder.transition_buffers(dst_barrier.into_iter());
            raw_encoder.copy_query_results(
                query_set.raw(),
                start_query..end_query,
                raw_dst_buffer,
                destination_offset,
                wgt::BufferSize::new_unchecked(stride as u64),
            );
        }

        Ok(())
    }
}
