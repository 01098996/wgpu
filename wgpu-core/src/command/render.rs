use crate::{
    binding_model::BindError,
    command::{
        bind::Binder,
        end_pipeline_statistics_query,
        memory_init::{fixup_discarded_surfaces, SurfacesInDiscardState},
        BasePass, BasePassRef, CommandBuffer, CommandEncoderError, CommandEncoderStatus, DrawError,
        ExecutionError, MapPassErr, PassErrorScope, QueryResetMap, QueryUseError, RenderCommand,
        RenderCommandError, StateChange,
    },
    device::{
        AttachmentData, Device, MissingDownlevelFlags, MissingFeatures,
        RenderPassCompatibilityError, RenderPassContext,
    },
    error::{ErrorFormatter, PrettyError},
    hub::{Global, GlobalIdentityHandlerFactory, HalApi, Storage, Token},
    id,
    init_tracker::{MemoryInitKind, TextureInitRange, TextureInitTrackerAction},
    pipeline::PipelineFlags,
    resource::{Texture, TextureView},
    track::{StatefulTrackerSubset, TextureSelector, UsageConflict},
    validation::{
        check_buffer_usage, check_texture_usage, MissingBufferUsageError, MissingTextureUsageError,
    },
    Label, Stored,
};

use arrayvec::ArrayVec;
use hal::CommandEncoder as _;
use thiserror::Error;
use wgt::{
    BufferAddress, BufferSize, BufferUsages, Color, IndexFormat, TextureUsages,
    TextureViewDimension, VertexStepMode,
};

#[cfg(any(feature = "serial-pass", feature = "replay"))]
use serde::Deserialize;
#[cfg(any(feature = "serial-pass", feature = "trace"))]
use serde::Serialize;

use crate::track::UseExtendError;
use std::{borrow::Cow, fmt, iter, marker::PhantomData, mem, num::NonZeroU32, ops::Range, str};

use super::{memory_init::TextureSurfaceDiscard, CommandBufferTextureMemoryActions};

/// Operation to perform to the output attachment at the start of a renderpass.
#[repr(C)]
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
#[cfg_attr(any(feature = "serial-pass", feature = "trace"), derive(Serialize))]
#[cfg_attr(any(feature = "serial-pass", feature = "replay"), derive(Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
pub enum LoadOp {
    /// Clear the output attachment with the clear color. Clearing is faster than loading.
    Clear = 0,
    /// Do not clear output attachment.
    Load = 1,
}

/// Operation to perform to the output attachment at the end of a renderpass.
#[repr(C)]
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
#[cfg_attr(any(feature = "serial-pass", feature = "trace"), derive(Serialize))]
#[cfg_attr(any(feature = "serial-pass", feature = "replay"), derive(Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
pub enum StoreOp {
    /// Discards the content of the render target. If you don't care about the contents of the target, this can be faster.
    Discard = 0,
    /// Store the result of the renderpass.
    Store = 1,
}

/// Describes an individual channel within a render pass, such as color, depth, or stencil.
#[repr(C)]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(any(feature = "serial-pass", feature = "trace"), derive(Serialize))]
#[cfg_attr(any(feature = "serial-pass", feature = "replay"), derive(Deserialize))]
pub struct PassChannel<V> {
    /// Operation to perform to the output attachment at the start of a renderpass. This must be clear if it
    /// is the first renderpass rendering to a swap chain image.
    pub load_op: LoadOp,
    /// Operation to perform to the output attachment at the end of a renderpass.
    pub store_op: StoreOp,
    /// If load_op is [`LoadOp::Clear`], the attachment will be cleared to this color.
    pub clear_value: V,
    /// If true, the relevant channel is not changed by a renderpass, and the corresponding attachment
    /// can be used inside the pass by other read-only usages.
    pub read_only: bool,
}

impl<V> PassChannel<V> {
    fn hal_ops(&self) -> hal::AttachmentOps {
        let mut ops = hal::AttachmentOps::empty();
        match self.load_op {
            LoadOp::Load => ops |= hal::AttachmentOps::LOAD,
            LoadOp::Clear => (),
        };
        match self.store_op {
            StoreOp::Store => ops |= hal::AttachmentOps::STORE,
            StoreOp::Discard => (),
        };
        ops
    }
}

/// Describes a color attachment to a render pass.
#[repr(C)]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(any(feature = "serial-pass", feature = "trace"), derive(Serialize))]
#[cfg_attr(any(feature = "serial-pass", feature = "replay"), derive(Deserialize))]
pub struct RenderPassColorAttachment {
    /// The view to use as an attachment.
    pub view: id::TextureViewId,
    /// The view that will receive the resolved output if multisampling is used.
    pub resolve_target: Option<id::TextureViewId>,
    /// What operations will be performed on this color attachment.
    pub channel: PassChannel<Color>,
}

/// Describes a depth/stencil attachment to a render pass.
#[repr(C)]
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(any(feature = "serial-pass", feature = "trace"), derive(Serialize))]
#[cfg_attr(any(feature = "serial-pass", feature = "replay"), derive(Deserialize))]
pub struct RenderPassDepthStencilAttachment {
    /// The view to use as an attachment.
    pub view: id::TextureViewId,
    /// What operations will be performed on the depth part of the attachment.
    pub depth: PassChannel<f32>,
    /// What operations will be performed on the stencil part of the attachment.
    pub stencil: PassChannel<u32>,
}

impl RenderPassDepthStencilAttachment {
    fn is_read_only(&self, aspects: hal::FormatAspects) -> Result<bool, RenderPassErrorInner> {
        if aspects.contains(hal::FormatAspects::DEPTH) && !self.depth.read_only {
            return Ok(false);
        }
        if (self.depth.load_op, self.depth.store_op) != (LoadOp::Load, StoreOp::Store) {
            return Err(RenderPassErrorInner::InvalidDepthOps);
        }
        if aspects.contains(hal::FormatAspects::STENCIL) && !self.stencil.read_only {
            return Ok(false);
        }
        if (self.stencil.load_op, self.stencil.store_op) != (LoadOp::Load, StoreOp::Store) {
            return Err(RenderPassErrorInner::InvalidStencilOps);
        }
        Ok(true)
    }
}

/// Describes the attachments of a render pass.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RenderPassDescriptor<'a> {
    pub label: Label<'a>,
    /// The color attachments of the render pass.
    pub color_attachments: Cow<'a, [RenderPassColorAttachment]>,
    /// The depth and stencil attachment of the render pass, if any.
    pub depth_stencil_attachment: Option<&'a RenderPassDepthStencilAttachment>,
}

#[cfg_attr(feature = "serial-pass", derive(Deserialize, Serialize))]
pub struct RenderPass {
    base: BasePass<RenderCommand>,
    parent_id: id::CommandEncoderId,
    color_targets: ArrayVec<RenderPassColorAttachment, { hal::MAX_COLOR_TARGETS }>,
    depth_stencil_target: Option<RenderPassDepthStencilAttachment>,
}

impl RenderPass {
    pub fn new(parent_id: id::CommandEncoderId, desc: &RenderPassDescriptor) -> Self {
        Self {
            base: BasePass::new(&desc.label),
            parent_id,
            color_targets: desc.color_attachments.iter().cloned().collect(),
            depth_stencil_target: desc.depth_stencil_attachment.cloned(),
        }
    }

    pub fn parent_id(&self) -> id::CommandEncoderId {
        self.parent_id
    }

    #[cfg(feature = "trace")]
    pub fn into_command(self) -> crate::device::trace::Command {
        crate::device::trace::Command::RunRenderPass {
            base: self.base,
            target_colors: self.color_targets.into_iter().collect(),
            target_depth_stencil: self.depth_stencil_target,
        }
    }

    pub fn set_index_buffer(
        &mut self,
        buffer_id: id::BufferId,
        index_format: IndexFormat,
        offset: BufferAddress,
        size: Option<BufferSize>,
    ) {
        self.base.commands.push(RenderCommand::SetIndexBuffer {
            buffer_id,
            index_format,
            offset,
            size,
        });
    }
}

impl fmt::Debug for RenderPass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RenderPass {{ encoder_id: {:?}, color_targets: {:?}, depth_stencil_target: {:?}, data: {:?} commands, {:?} dynamic offsets, and {:?} push constant u32s }}",
            self.parent_id,
            self.color_targets,
            self.depth_stencil_target,
            self.base.commands.len(),
            self.base.dynamic_offsets.len(),
            self.base.push_constant_data.len(),
        )
    }
}

#[derive(Debug, PartialEq)]
enum OptionalState {
    Unused,
    Required,
    Set,
}

impl OptionalState {
    fn require(&mut self, require: bool) {
        if require && *self == Self::Unused {
            *self = Self::Required;
        }
    }
}

#[derive(Debug, Default)]
struct IndexState {
    bound_buffer_view: Option<(id::Valid<id::BufferId>, Range<BufferAddress>)>,
    format: Option<IndexFormat>,
    pipeline_format: Option<IndexFormat>,
    limit: u32,
}

impl IndexState {
    fn update_limit(&mut self) {
        self.limit = match self.bound_buffer_view {
            Some((_, ref range)) => {
                let format = self
                    .format
                    .expect("IndexState::update_limit must be called after a index buffer is set");
                let shift = match format {
                    IndexFormat::Uint16 => 1,
                    IndexFormat::Uint32 => 2,
                };
                ((range.end - range.start) >> shift) as u32
            }
            None => 0,
        }
    }

    fn reset(&mut self) {
        self.bound_buffer_view = None;
        self.limit = 0;
    }
}

#[derive(Clone, Copy, Debug)]
struct VertexBufferState {
    total_size: BufferAddress,
    stride: BufferAddress,
    rate: VertexStepMode,
    bound: bool,
}

impl VertexBufferState {
    const EMPTY: Self = Self {
        total_size: 0,
        stride: 0,
        rate: VertexStepMode::Vertex,
        bound: false,
    };
}

#[derive(Debug, Default)]
struct VertexState {
    inputs: ArrayVec<VertexBufferState, { hal::MAX_VERTEX_BUFFERS }>,
    /// Length of the shortest vertex rate vertex buffer
    vertex_limit: u32,
    /// Buffer slot which the shortest vertex rate vertex buffer is bound to
    vertex_limit_slot: u32,
    /// Length of the shortest instance rate vertex buffer
    instance_limit: u32,
    /// Buffer slot which the shortest instance rate vertex buffer is bound to
    instance_limit_slot: u32,
    /// Total amount of buffers required by the pipeline.
    buffers_required: u32,
}

impl VertexState {
    fn update_limits(&mut self) {
        self.vertex_limit = u32::MAX;
        self.instance_limit = u32::MAX;
        for (idx, vbs) in self.inputs.iter().enumerate() {
            if vbs.stride == 0 || !vbs.bound {
                continue;
            }
            let limit = (vbs.total_size / vbs.stride) as u32;
            match vbs.rate {
                VertexStepMode::Vertex => {
                    if limit < self.vertex_limit {
                        self.vertex_limit = limit;
                        self.vertex_limit_slot = idx as _;
                    }
                }
                VertexStepMode::Instance => {
                    if limit < self.instance_limit {
                        self.instance_limit = limit;
                        self.instance_limit_slot = idx as _;
                    }
                }
            }
        }
    }

    fn reset(&mut self) {
        self.inputs.clear();
        self.vertex_limit = 0;
        self.instance_limit = 0;
    }
}

#[derive(Debug)]
struct State {
    pipeline_flags: PipelineFlags,
    binder: Binder,
    blend_constant: OptionalState,
    stencil_reference: u32,
    pipeline: StateChange<id::RenderPipelineId>,
    index: IndexState,
    vertex: VertexState,
    debug_scope_depth: u32,
}

impl State {
    fn is_ready(&self, indexed: bool) -> Result<(), DrawError> {
        // Determine how many vertex buffers have already been bound
        let vertex_buffer_count = self.vertex.inputs.iter().take_while(|v| v.bound).count() as u32;
        // Compare with the needed quantity
        if vertex_buffer_count < self.vertex.buffers_required {
            return Err(DrawError::MissingVertexBuffer {
                index: vertex_buffer_count,
            });
        }

        let bind_mask = self.binder.invalid_mask();
        if bind_mask != 0 {
            //let (expected, provided) = self.binder.entries[index as usize].info();
            return Err(DrawError::IncompatibleBindGroup {
                index: bind_mask.trailing_zeros(),
            });
        }
        if self.pipeline.is_unset() {
            return Err(DrawError::MissingPipeline);
        }
        if self.blend_constant == OptionalState::Required {
            return Err(DrawError::MissingBlendConstant);
        }

        if indexed {
            // Pipeline expects an index buffer
            if let Some(pipeline_index_format) = self.index.pipeline_format {
                // We have a buffer bound
                let buffer_index_format = self.index.format.ok_or(DrawError::MissingIndexBuffer)?;

                // The buffers are different formats
                if pipeline_index_format != buffer_index_format {
                    return Err(DrawError::UnmatchedIndexFormats {
                        pipeline: pipeline_index_format,
                        buffer: buffer_index_format,
                    });
                }
            }
        }

        self.binder.check_late_buffer_bindings()?;

        Ok(())
    }

    /// Reset the `RenderBundle`-related states.
    fn reset_bundle(&mut self) {
        self.binder.reset();
        self.pipeline.reset();
        self.index.reset();
        self.vertex.reset();
    }
}

/// Error encountered when performing a render pass.
#[derive(Clone, Debug, Error)]
pub enum RenderPassErrorInner {
    #[error(transparent)]
    Encoder(#[from] CommandEncoderError),
    #[error("attachment texture view {0:?} is invalid")]
    InvalidAttachment(id::TextureViewId),
    #[error("attachment format {0:?} is not a color format")]
    InvalidColorAttachmentFormat(wgt::TextureFormat),
    #[error("attachment format {0:?} is not a depth-stencil format")]
    InvalidDepthStencilAttachmentFormat(wgt::TextureFormat),
    #[error("attachment format {0:?} can not be resolved")]
    UnsupportedResolveTargetFormat(wgt::TextureFormat),
    #[error("necessary attachments are missing")]
    MissingAttachments,
    #[error("attachments have differing sizes: {previous:?} is followed by {mismatch:?}")]
    AttachmentsDimensionMismatch {
        previous: (&'static str, wgt::Extent3d),
        mismatch: (&'static str, wgt::Extent3d),
    },
    #[error("attachment's sample count {0} is invalid")]
    InvalidSampleCount(u32),
    #[error("resolve source must be multi-sampled (has {src} samples) while the resolve destination must not be multisampled (has {dst} samples)")]
    InvalidResolveSampleCounts { src: u32, dst: u32 },
    #[error(
        "resource source format ({src:?}) must match the resolve destination format ({dst:?})"
    )]
    MismatchedResolveTextureFormat {
        src: wgt::TextureFormat,
        dst: wgt::TextureFormat,
    },
    #[error("surface texture is dropped before the render pass is finished")]
    SurfaceTextureDropped,
    #[error("not enough memory left")]
    OutOfMemory,
    #[error("unable to clear non-present/read-only depth")]
    InvalidDepthOps,
    #[error("unable to clear non-present/read-only stencil")]
    InvalidStencilOps,
    #[error("all attachments must have the same sample count, found {actual} != {expected}")]
    SampleCountMismatch { actual: u32, expected: u32 },
    #[error("setting `values_offset` to be `None` is only for internal use in render bundles")]
    InvalidValuesOffset,
    #[error(transparent)]
    MissingFeatures(#[from] MissingFeatures),
    #[error(transparent)]
    MissingDownlevelFlags(#[from] MissingDownlevelFlags),
    #[error("indirect draw uses bytes {offset}..{end_offset} {} which overruns indirect buffer of size {buffer_size}",
        count.map_or_else(String::new, |v| format!("(using count {})", v)))]
    IndirectBufferOverrun {
        count: Option<NonZeroU32>,
        offset: u64,
        end_offset: u64,
        buffer_size: u64,
    },
    #[error("indirect draw uses bytes {begin_count_offset}..{end_count_offset} which overruns indirect buffer of size {count_buffer_size}")]
    IndirectCountBufferOverrun {
        begin_count_offset: u64,
        end_count_offset: u64,
        count_buffer_size: u64,
    },
    #[error("cannot pop debug group, because number of pushed debug groups is zero")]
    InvalidPopDebugGroup,
    #[error(transparent)]
    ResourceUsageConflict(#[from] UsageConflict),
    #[error("render bundle has incompatible targets, {0}")]
    IncompatibleBundleTargets(#[from] RenderPassCompatibilityError),
    #[error("render bundle has an incompatible read-only depth/stencil flag: bundle is {bundle}, while the pass is {pass}")]
    IncompatibleBundleRods { pass: bool, bundle: bool },
    #[error(transparent)]
    RenderCommand(#[from] RenderCommandError),
    #[error(transparent)]
    Draw(#[from] DrawError),
    #[error(transparent)]
    Bind(#[from] BindError),
    #[error(transparent)]
    QueryUse(#[from] QueryUseError),
    #[error("multiview layer count must match")]
    MultiViewMismatch,
    #[error(
        "multiview pass texture views with more than one array layer must have D2Array dimension"
    )]
    MultiViewDimensionMismatch,
}

impl PrettyError for RenderPassErrorInner {
    fn fmt_pretty(&self, fmt: &mut ErrorFormatter) {
        fmt.error(self);
        if let Self::InvalidAttachment(id) = *self {
            fmt.texture_view_label_with_key(&id, "attachment");
        };
    }
}

impl From<MissingBufferUsageError> for RenderPassErrorInner {
    fn from(error: MissingBufferUsageError) -> Self {
        Self::RenderCommand(error.into())
    }
}

impl From<MissingTextureUsageError> for RenderPassErrorInner {
    fn from(error: MissingTextureUsageError) -> Self {
        Self::RenderCommand(error.into())
    }
}

/// Error encountered when performing a render pass.
#[derive(Clone, Debug, Error)]
#[error("{scope}")]
pub struct RenderPassError {
    pub scope: PassErrorScope,
    #[source]
    inner: RenderPassErrorInner,
}
impl PrettyError for RenderPassError {
    fn fmt_pretty(&self, fmt: &mut ErrorFormatter) {
        // This error is wrapper for the inner error,
        // but the scope has useful labels
        fmt.error(self);
        self.scope.fmt_pretty(fmt);
    }
}

impl<T, E> MapPassErr<T, RenderPassError> for Result<T, E>
where
    E: Into<RenderPassErrorInner>,
{
    fn map_pass_err(self, scope: PassErrorScope) -> Result<T, RenderPassError> {
        self.map_err(|inner| RenderPassError {
            scope,
            inner: inner.into(),
        })
    }
}

struct RenderAttachment<'a> {
    texture_id: &'a Stored<id::TextureId>,
    selector: &'a TextureSelector,
    usage: hal::TextureUses,
}

impl<A: hal::Api> TextureView<A> {
    fn to_render_attachment(&self, usage: hal::TextureUses) -> RenderAttachment {
        RenderAttachment {
            texture_id: &self.parent_id,
            selector: &self.selector,
            usage,
        }
    }
}

const MAX_TOTAL_ATTACHMENTS: usize = hal::MAX_COLOR_TARGETS + hal::MAX_COLOR_TARGETS + 1;
type AttachmentDataVec<T> = ArrayVec<T, MAX_TOTAL_ATTACHMENTS>;

struct RenderPassInfo<'a, A: hal::Api> {
    context: RenderPassContext,
    trackers: StatefulTrackerSubset,
    render_attachments: AttachmentDataVec<RenderAttachment<'a>>, // All render attachments, including depth/stencil
    is_ds_read_only: bool,
    extent: wgt::Extent3d,
    _phantom: PhantomData<A>,

    pending_discard_init_fixups: SurfacesInDiscardState,
    divergent_discarded_depth_stencil_aspect: Option<(wgt::TextureAspect, &'a TextureView<A>)>,
    multiview: Option<NonZeroU32>,
}

impl<'a, A: HalApi> RenderPassInfo<'a, A> {
    fn add_pass_texture_init_actions<V>(
        channel: &PassChannel<V>,
        texture_memory_actions: &mut CommandBufferTextureMemoryActions,
        view: &TextureView<A>,
        texture_guard: &Storage<Texture<A>, id::TextureId>,
        pending_discard_init_fixups: &mut SurfacesInDiscardState,
    ) {
        if channel.load_op == LoadOp::Load {
            pending_discard_init_fixups.extend(texture_memory_actions.register_init_action(
                &TextureInitTrackerAction {
                    id: view.parent_id.value.0,
                    range: TextureInitRange::from(view.selector.clone()),
                    // Note that this is needed even if the target is discarded,
                    kind: MemoryInitKind::NeedsInitializedMemory,
                },
                texture_guard,
            ));
        } else if channel.store_op == StoreOp::Store {
            // Clear + Store
            texture_memory_actions.register_implicit_init(
                view.parent_id.value,
                TextureInitRange::from(view.selector.clone()),
                texture_guard,
            );
        }
        if channel.store_op == StoreOp::Discard {
            // the discard happens at the *end* of a pass
            // but recording the discard right away be alright since the texture can't be used during the pass anyways
            texture_memory_actions.discard(TextureSurfaceDiscard {
                texture: view.parent_id.value.0,
                mip_level: view.selector.levels.start,
                layer: view.selector.layers.start,
            });
        }
    }

    fn start(
        device: &Device<A>,
        label: Option<&str>,
        color_attachments: &[RenderPassColorAttachment],
        depth_stencil_attachment: Option<&RenderPassDepthStencilAttachment>,
        cmd_buf: &mut CommandBuffer<A>,
        view_guard: &'a Storage<TextureView<A>, id::TextureViewId>,
        texture_guard: &'a Storage<Texture<A>, id::TextureId>,
    ) -> Result<Self, RenderPassErrorInner> {
        profiling::scope!("start", "RenderPassInfo");

        // We default to false intentionally, even if depth-stencil isn't used at all.
        // This allows us to use the primary raw pipeline in `RenderPipeline`,
        // instead of the special read-only one, which would be `None`.
        let mut is_ds_read_only = false;

        let mut render_attachments = AttachmentDataVec::<RenderAttachment>::new();
        let mut discarded_surfaces = AttachmentDataVec::new();
        let mut pending_discard_init_fixups = SurfacesInDiscardState::new();
        let mut divergent_discarded_depth_stencil_aspect = None;

        let mut attachment_type_name = "";
        let mut extent = None;
        let mut sample_count = 0;

        let mut detected_multiview: Option<Option<NonZeroU32>> = None;

        let mut check_multiview = |view: &TextureView<A>| {
            // Get the multiview configuration for this texture view
            let layers = view.selector.layers.end - view.selector.layers.start;
            let this_multiview = if layers >= 2 {
                // Trivially proven by the if above
                Some(unsafe { NonZeroU32::new_unchecked(layers) })
            } else {
                None
            };

            // Make sure that if this view is a multiview, it is set to be an array
            if this_multiview.is_some() && view.desc.dimension != TextureViewDimension::D2Array {
                return Err(RenderPassErrorInner::MultiViewDimensionMismatch);
            }

            // Validate matching first, or store the first one
            if let Some(multiview) = detected_multiview {
                if multiview != this_multiview {
                    return Err(RenderPassErrorInner::MultiViewMismatch);
                }
            } else {
                // Multiview is only supported if the feature is enabled
                if this_multiview.is_some() {
                    device.require_features(wgt::Features::MULTIVIEW)?;
                }

                detected_multiview = Some(this_multiview);
            }

            Ok(())
        };
        let mut add_view = |view: &TextureView<A>, type_name| {
            if let Some(ex) = extent {
                if ex != view.extent {
                    return Err(RenderPassErrorInner::AttachmentsDimensionMismatch {
                        previous: (attachment_type_name, ex),
                        mismatch: (type_name, view.extent),
                    });
                }
            } else {
                extent = Some(view.extent);
            }
            if sample_count == 0 {
                sample_count = view.samples;
            } else if sample_count != view.samples {
                return Err(RenderPassErrorInner::SampleCountMismatch {
                    actual: view.samples,
                    expected: sample_count,
                });
            }
            attachment_type_name = type_name;
            Ok(())
        };

        let mut colors = ArrayVec::<hal::ColorAttachment<A>, { hal::MAX_COLOR_TARGETS }>::new();
        let mut depth_stencil = None;

        if let Some(at) = depth_stencil_attachment {
            let view = cmd_buf
                .trackers
                .views
                .use_extend(&*view_guard, at.view, (), ())
                .map_err(|_| RenderPassErrorInner::InvalidAttachment(at.view))?;
            check_multiview(view)?;
            add_view(view, "depth")?;

            let ds_aspects = view.desc.aspects();
            if ds_aspects.contains(hal::FormatAspects::COLOR) {
                return Err(RenderPassErrorInner::InvalidDepthStencilAttachmentFormat(
                    view.desc.format,
                ));
            }

            if !ds_aspects.contains(hal::FormatAspects::STENCIL)
                || (at.stencil.load_op == at.depth.load_op
                    && at.stencil.store_op == at.depth.store_op)
            {
                Self::add_pass_texture_init_actions(
                    &at.depth,
                    &mut cmd_buf.texture_memory_actions,
                    view,
                    texture_guard,
                    &mut pending_discard_init_fixups,
                );
            } else if !ds_aspects.contains(hal::FormatAspects::DEPTH) {
                Self::add_pass_texture_init_actions(
                    &at.stencil,
                    &mut cmd_buf.texture_memory_actions,
                    view,
                    texture_guard,
                    &mut pending_discard_init_fixups,
                );
            } else {
                // This is the only place (anywhere in wgpu) where Stencil & Depth init state can diverge.
                // To safe us the overhead of tracking init state of texture aspects everywhere,
                // we're going to cheat a little bit in order to keep the init state of both Stencil and Depth aspects in sync.
                // The expectation is that we hit this path extremely rarely!

                // Diverging LoadOp, i.e. Load + Clear:
                // Record MemoryInitKind::NeedsInitializedMemory for the entire surface, a bit wasteful on unit but no negative effect!
                // Rationale: If the loaded channel is uninitialized it needs clearing, the cleared channel doesn't care. (If everything is already initialized nothing special happens)
                // (possible minor optimization: Clear caused by NeedsInitializedMemory should know that it doesn't need to clear the aspect that was set to C)
                let need_init_beforehand =
                    at.depth.load_op == LoadOp::Load || at.stencil.load_op == LoadOp::Load;
                if need_init_beforehand {
                    pending_discard_init_fixups.extend(
                        cmd_buf.texture_memory_actions.register_init_action(
                            &TextureInitTrackerAction {
                                id: view.parent_id.value.0,
                                range: TextureInitRange::from(view.selector.clone()),
                                kind: MemoryInitKind::NeedsInitializedMemory,
                            },
                            texture_guard,
                        ),
                    );
                }

                // Diverging Store, i.e. Discard + Store:
                // Immediately zero out channel that is set to discard after we're done with the render pass.
                // This allows us to set the entire surface to MemoryInitKind::ImplicitlyInitialized (if it isn't already set to NeedsInitializedMemory).
                // (possible optimization: Delay and potentially drop this zeroing)
                if at.depth.store_op != at.stencil.store_op {
                    if !need_init_beforehand {
                        cmd_buf.texture_memory_actions.register_implicit_init(
                            view.parent_id.value,
                            TextureInitRange::from(view.selector.clone()),
                            texture_guard,
                        );
                    }
                    divergent_discarded_depth_stencil_aspect = Some((
                        if at.depth.store_op == StoreOp::Discard {
                            wgt::TextureAspect::DepthOnly
                        } else {
                            wgt::TextureAspect::StencilOnly
                        },
                        view,
                    ));
                } else if at.depth.store_op == StoreOp::Discard {
                    // Both are discarded using the regular path.
                    discarded_surfaces.push(TextureSurfaceDiscard {
                        texture: view.parent_id.value.0,
                        mip_level: view.selector.levels.start,
                        layer: view.selector.layers.start,
                    });
                }
            }

            let usage = if at.is_read_only(ds_aspects)? {
                is_ds_read_only = true;
                hal::TextureUses::DEPTH_STENCIL_READ | hal::TextureUses::RESOURCE
            } else {
                hal::TextureUses::DEPTH_STENCIL_WRITE
            };
            render_attachments.push(view.to_render_attachment(usage));

            depth_stencil = Some(hal::DepthStencilAttachment {
                target: hal::Attachment {
                    view: &view.raw,
                    usage,
                },
                depth_ops: at.depth.hal_ops(),
                stencil_ops: at.stencil.hal_ops(),
                clear_value: (at.depth.clear_value, at.stencil.clear_value),
            });
        }

        for at in color_attachments {
            let color_view = cmd_buf
                .trackers
                .views
                .use_extend(&*view_guard, at.view, (), ())
                .map_err(|_| RenderPassErrorInner::InvalidAttachment(at.view))?;
            check_multiview(color_view)?;
            add_view(color_view, "color")?;

            if !color_view
                .desc
                .aspects()
                .contains(hal::FormatAspects::COLOR)
            {
                return Err(RenderPassErrorInner::InvalidColorAttachmentFormat(
                    color_view.desc.format,
                ));
            }

            Self::add_pass_texture_init_actions(
                &at.channel,
                &mut cmd_buf.texture_memory_actions,
                color_view,
                texture_guard,
                &mut pending_discard_init_fixups,
            );
            render_attachments
                .push(color_view.to_render_attachment(hal::TextureUses::COLOR_TARGET));

            let mut hal_resolve_target = None;
            if let Some(resolve_target) = at.resolve_target {
                let resolve_view = cmd_buf
                    .trackers
                    .views
                    .use_extend(&*view_guard, resolve_target, (), ())
                    .map_err(|_| RenderPassErrorInner::InvalidAttachment(resolve_target))?;

                check_multiview(resolve_view)?;
                if color_view.extent != resolve_view.extent {
                    return Err(RenderPassErrorInner::AttachmentsDimensionMismatch {
                        previous: (attachment_type_name, extent.unwrap_or_default()),
                        mismatch: ("resolve", resolve_view.extent),
                    });
                }
                if color_view.samples == 1 || resolve_view.samples != 1 {
                    return Err(RenderPassErrorInner::InvalidResolveSampleCounts {
                        src: color_view.samples,
                        dst: resolve_view.samples,
                    });
                }
                if color_view.desc.format != resolve_view.desc.format {
                    return Err(RenderPassErrorInner::MismatchedResolveTextureFormat {
                        src: color_view.desc.format,
                        dst: resolve_view.desc.format,
                    });
                }
                if !resolve_view
                    .format_features
                    .flags
                    .contains(wgt::TextureFormatFeatureFlags::MULTISAMPLE_RESOLVE)
                {
                    return Err(RenderPassErrorInner::UnsupportedResolveTargetFormat(
                        resolve_view.desc.format,
                    ));
                }

                cmd_buf.texture_memory_actions.register_implicit_init(
                    resolve_view.parent_id.value,
                    TextureInitRange::from(resolve_view.selector.clone()),
                    texture_guard,
                );
                render_attachments
                    .push(resolve_view.to_render_attachment(hal::TextureUses::COLOR_TARGET));

                hal_resolve_target = Some(hal::Attachment {
                    view: &resolve_view.raw,
                    usage: hal::TextureUses::COLOR_TARGET,
                });
            }

            colors.push(hal::ColorAttachment {
                target: hal::Attachment {
                    view: &color_view.raw,
                    usage: hal::TextureUses::COLOR_TARGET,
                },
                resolve_target: hal_resolve_target,
                ops: at.channel.hal_ops(),
                clear_value: at.channel.clear_value,
            });
        }

        if sample_count != 1 && sample_count != 4 {
            return Err(RenderPassErrorInner::InvalidSampleCount(sample_count));
        }

        let view_data = AttachmentData {
            colors: color_attachments
                .iter()
                .map(|at| view_guard.get(at.view).unwrap())
                .collect(),
            resolves: color_attachments
                .iter()
                .filter_map(|at| at.resolve_target)
                .map(|attachment| view_guard.get(attachment).unwrap())
                .collect(),
            depth_stencil: depth_stencil_attachment.map(|at| view_guard.get(at.view).unwrap()),
        };
        let extent = extent.ok_or(RenderPassErrorInner::MissingAttachments)?;

        let multiview = detected_multiview.expect("Multiview was not detected, no attachments");
        let context = RenderPassContext {
            attachments: view_data.map(|view| view.desc.format),
            sample_count,
            multiview,
        };

        let hal_desc = hal::RenderPassDescriptor {
            label,
            extent,
            sample_count,
            color_attachments: &colors,
            depth_stencil_attachment: depth_stencil,
            multiview,
        };
        unsafe {
            cmd_buf.encoder.raw.begin_render_pass(&hal_desc);
        };

        Ok(Self {
            context,
            trackers: StatefulTrackerSubset::new(A::VARIANT),
            render_attachments,
            is_ds_read_only,
            extent,
            _phantom: PhantomData,
            pending_discard_init_fixups,
            divergent_discarded_depth_stencil_aspect,
            multiview,
        })
    }

    fn finish(
        mut self,
        raw: &mut A::CommandEncoder,
        texture_guard: &Storage<Texture<A>, id::TextureId>,
    ) -> Result<(StatefulTrackerSubset, SurfacesInDiscardState), RenderPassErrorInner> {
        profiling::scope!("finish", "RenderPassInfo");
        unsafe {
            raw.end_render_pass();
        }

        for ra in self.render_attachments {
            if !texture_guard.contains(ra.texture_id.value.0) {
                return Err(RenderPassErrorInner::SurfaceTextureDropped);
            }
            let texture = &texture_guard[ra.texture_id.value];
            check_texture_usage(texture.desc.usage, TextureUsages::RENDER_ATTACHMENT)?;

            // the tracker set of the pass is always in "extend" mode
            self.trackers
                .textures
                .change_extend(
                    ra.texture_id.value,
                    &ra.texture_id.ref_count,
                    ra.selector.clone(),
                    ra.usage,
                )
                .map_err(UsageConflict::from)?;
        }

        // If either only stencil or depth was discarded, we put in a special clear pass to keep the init status of the aspects in sync.
        // We do this so we don't need to track init state for depth/stencil aspects individually.
        // Note that we don't go the usual route of "brute force" initializing the texture when need arises here,
        // since this path is actually something a user may genuinely want (where as the other cases are more seen along the lines as gracefully handling a user error).
        if let Some((aspect, view)) = self.divergent_discarded_depth_stencil_aspect {
            let (depth_ops, stencil_ops) = if aspect == wgt::TextureAspect::DepthOnly {
                (
                    hal::AttachmentOps::STORE,                            // clear depth
                    hal::AttachmentOps::LOAD | hal::AttachmentOps::STORE, // unchanged stencil
                )
            } else {
                (
                    hal::AttachmentOps::LOAD | hal::AttachmentOps::STORE, // unchanged stencil
                    hal::AttachmentOps::STORE,                            // clear depth
                )
            };
            let desc = hal::RenderPassDescriptor {
                label: Some("(wgpu internal) Zero init discarded depth/stencil aspect"),
                extent: view.extent,
                sample_count: view.samples,
                color_attachments: &[],
                depth_stencil_attachment: Some(hal::DepthStencilAttachment {
                    target: hal::Attachment {
                        view: &view.raw,
                        usage: hal::TextureUses::DEPTH_STENCIL_WRITE,
                    },
                    depth_ops,
                    stencil_ops,
                    clear_value: (0.0, 0),
                }),
                multiview: self.multiview,
            };
            unsafe {
                raw.begin_render_pass(&desc);
                raw.end_render_pass();
            }
        }

        Ok((self.trackers, self.pending_discard_init_fixups))
    }
}

// Common routines between render/compute

impl<G: GlobalIdentityHandlerFactory> Global<G> {
    pub fn command_encoder_run_render_pass<A: HalApi>(
        &self,
        encoder_id: id::CommandEncoderId,
        pass: &RenderPass,
    ) -> Result<(), RenderPassError> {
        self.command_encoder_run_render_pass_impl::<A>(
            encoder_id,
            pass.base.as_ref(),
            &pass.color_targets,
            pass.depth_stencil_target.as_ref(),
        )
    }

    #[doc(hidden)]
    pub fn command_encoder_run_render_pass_impl<A: HalApi>(
        &self,
        encoder_id: id::CommandEncoderId,
        base: BasePassRef<RenderCommand>,
        color_attachments: &[RenderPassColorAttachment],
        depth_stencil_attachment: Option<&RenderPassDepthStencilAttachment>,
    ) -> Result<(), RenderPassError> {
        profiling::scope!("run_render_pass", "CommandEncoder");
        let init_scope = PassErrorScope::Pass(encoder_id);

        let hub = A::hub(self);
        let mut token = Token::root();
        let (device_guard, mut token) = hub.devices.read(&mut token);

        let (trackers, query_reset_state, pending_discard_init_fixups) = {
            let (mut cmb_guard, mut token) = hub.command_buffers.write(&mut token);

            let cmd_buf = CommandBuffer::get_encoder_mut(&mut *cmb_guard, encoder_id)
                .map_pass_err(init_scope)?;
            // close everything while the new command encoder is filled
            cmd_buf.encoder.close();
            // will be reset to true if recording is done without errors
            cmd_buf.status = CommandEncoderStatus::Error;

            #[cfg(feature = "trace")]
            if let Some(ref mut list) = cmd_buf.commands {
                list.push(crate::device::trace::Command::RunRenderPass {
                    base: BasePass::from_ref(base),
                    target_colors: color_attachments.to_vec(),
                    target_depth_stencil: depth_stencil_attachment.cloned(),
                });
            }

            let device = &device_guard[cmd_buf.device_id.value];
            cmd_buf.encoder.open_pass(base.label);

            let (bundle_guard, mut token) = hub.render_bundles.read(&mut token);
            let (pipeline_layout_guard, mut token) = hub.pipeline_layouts.read(&mut token);
            let (bind_group_guard, mut token) = hub.bind_groups.read(&mut token);
            let (pipeline_guard, mut token) = hub.render_pipelines.read(&mut token);
            let (query_set_guard, mut token) = hub.query_sets.read(&mut token);
            let (buffer_guard, mut token) = hub.buffers.read(&mut token);
            let (texture_guard, mut token) = hub.textures.read(&mut token);
            let (view_guard, _) = hub.texture_views.read(&mut token);

            log::trace!(
                "Encoding render pass begin in command buffer {:?}",
                encoder_id
            );

            let mut info = RenderPassInfo::start(
                device,
                base.label,
                color_attachments,
                depth_stencil_attachment,
                cmd_buf,
                &*view_guard,
                &*texture_guard,
            )
            .map_pass_err(init_scope)?;

            let raw = &mut cmd_buf.encoder.raw;

            let mut state = State {
                pipeline_flags: PipelineFlags::empty(),
                binder: Binder::new(),
                blend_constant: OptionalState::Unused,
                stencil_reference: 0,
                pipeline: StateChange::new(),
                index: IndexState::default(),
                vertex: VertexState::default(),
                debug_scope_depth: 0,
            };
            let mut temp_offsets = Vec::new();
            let mut dynamic_offset_count = 0;
            let mut string_offset = 0;
            let mut active_query = None;
            let mut query_reset_state = QueryResetMap::new();

            for command in base.commands {
                match *command {
                    RenderCommand::SetBindGroup {
                        index,
                        num_dynamic_offsets,
                        bind_group_id,
                    } => {
                        let scope = PassErrorScope::SetBindGroup(bind_group_id);
                        let max_bind_groups = device.limits.max_bind_groups;
                        if (index as u32) >= max_bind_groups {
                            return Err(RenderCommandError::BindGroupIndexOutOfRange {
                                index,
                                max: max_bind_groups,
                            })
                            .map_pass_err(scope);
                        }

                        temp_offsets.clear();
                        temp_offsets.extend_from_slice(
                            &base.dynamic_offsets[dynamic_offset_count
                                ..dynamic_offset_count + (num_dynamic_offsets as usize)],
                        );
                        dynamic_offset_count += num_dynamic_offsets as usize;

                        let bind_group = cmd_buf
                            .trackers
                            .bind_groups
                            .use_extend(&*bind_group_guard, bind_group_id, (), ())
                            .map_err(|_| RenderCommandError::InvalidBindGroup(bind_group_id))
                            .map_pass_err(scope)?;
                        bind_group
                            .validate_dynamic_bindings(&temp_offsets, &cmd_buf.limits)
                            .map_pass_err(scope)?;

                        // merge the resource tracker in
                        info.trackers
                            .merge_extend(&bind_group.used)
                            .map_pass_err(scope)?;
                        //Note: stateless trackers are not merged: the lifetime reference
                        // is held to the bind group itself.

                        cmd_buf.buffer_memory_init_actions.extend(
                            bind_group.used_buffer_ranges.iter().filter_map(|action| {
                                match buffer_guard.get(action.id) {
                                    Ok(buffer) => buffer.initialization_status.check_action(action),
                                    Err(_) => None,
                                }
                            }),
                        );
                        for action in bind_group.used_texture_ranges.iter() {
                            info.pending_discard_init_fixups.extend(
                                cmd_buf
                                    .texture_memory_actions
                                    .register_init_action(action, &texture_guard),
                            );
                        }

                        let pipeline_layout_id = state.binder.pipeline_layout_id;
                        let entries = state.binder.assign_group(
                            index as usize,
                            id::Valid(bind_group_id),
                            bind_group,
                            &temp_offsets,
                        );
                        if !entries.is_empty() {
                            let pipeline_layout =
                                &pipeline_layout_guard[pipeline_layout_id.unwrap()].raw;
                            for (i, e) in entries.iter().enumerate() {
                                let raw_bg =
                                    &bind_group_guard[e.group_id.as_ref().unwrap().value].raw;

                                unsafe {
                                    raw.set_bind_group(
                                        pipeline_layout,
                                        index as u32 + i as u32,
                                        raw_bg,
                                        &e.dynamic_offsets,
                                    );
                                }
                            }
                        }
                    }
                    RenderCommand::SetPipeline(pipeline_id) => {
                        let scope = PassErrorScope::SetPipelineRender(pipeline_id);
                        if state.pipeline.set_and_check_redundant(pipeline_id) {
                            continue;
                        }

                        let pipeline = cmd_buf
                            .trackers
                            .render_pipes
                            .use_extend(&*pipeline_guard, pipeline_id, (), ())
                            .map_err(|_| RenderCommandError::InvalidPipeline(pipeline_id))
                            .map_pass_err(scope)?;

                        info.context
                            .check_compatible(&pipeline.pass_context)
                            .map_err(RenderCommandError::IncompatiblePipelineTargets)
                            .map_pass_err(scope)?;

                        state.pipeline_flags = pipeline.flags;

                        if pipeline.flags.contains(PipelineFlags::WRITES_DEPTH_STENCIL)
                            && info.is_ds_read_only
                        {
                            return Err(RenderCommandError::IncompatiblePipelineRods)
                                .map_pass_err(scope);
                        }

                        state
                            .blend_constant
                            .require(pipeline.flags.contains(PipelineFlags::BLEND_CONSTANT));

                        unsafe {
                            raw.set_render_pipeline(&pipeline.raw);
                        }

                        if pipeline.flags.contains(PipelineFlags::STENCIL_REFERENCE) {
                            unsafe {
                                raw.set_stencil_reference(state.stencil_reference);
                            }
                        }

                        // Rebind resource
                        if state.binder.pipeline_layout_id != Some(pipeline.layout_id.value) {
                            let pipeline_layout = &pipeline_layout_guard[pipeline.layout_id.value];

                            let (start_index, entries) = state.binder.change_pipeline_layout(
                                &*pipeline_layout_guard,
                                pipeline.layout_id.value,
                                &pipeline.late_sized_buffer_groups,
                            );
                            if !entries.is_empty() {
                                for (i, e) in entries.iter().enumerate() {
                                    let raw_bg =
                                        &bind_group_guard[e.group_id.as_ref().unwrap().value].raw;

                                    unsafe {
                                        raw.set_bind_group(
                                            &pipeline_layout.raw,
                                            start_index as u32 + i as u32,
                                            raw_bg,
                                            &e.dynamic_offsets,
                                        );
                                    }
                                }
                            }

                            // Clear push constant ranges
                            let non_overlapping = super::bind::compute_nonoverlapping_ranges(
                                &pipeline_layout.push_constant_ranges,
                            );
                            for range in non_overlapping {
                                let offset = range.range.start;
                                let size_bytes = range.range.end - offset;
                                super::push_constant_clear(
                                    offset,
                                    size_bytes,
                                    |clear_offset, clear_data| unsafe {
                                        raw.set_push_constants(
                                            &pipeline_layout.raw,
                                            range.stages,
                                            clear_offset,
                                            clear_data,
                                        );
                                    },
                                );
                            }
                        }

                        state.index.pipeline_format = pipeline.strip_index_format;

                        let vertex_strides_len = pipeline.vertex_strides.len();
                        state.vertex.buffers_required = vertex_strides_len as u32;

                        while state.vertex.inputs.len() < vertex_strides_len {
                            state.vertex.inputs.push(VertexBufferState::EMPTY);
                        }

                        // Update vertex buffer limits
                        for (vbs, &(stride, rate)) in
                            state.vertex.inputs.iter_mut().zip(&pipeline.vertex_strides)
                        {
                            vbs.stride = stride;
                            vbs.rate = rate;
                        }
                        for vbs in state.vertex.inputs.iter_mut().skip(vertex_strides_len) {
                            vbs.stride = 0;
                            vbs.rate = VertexStepMode::Vertex;
                        }
                        state.vertex.update_limits();
                    }
                    RenderCommand::SetIndexBuffer {
                        buffer_id,
                        index_format,
                        offset,
                        size,
                    } => {
                        let scope = PassErrorScope::SetIndexBuffer(buffer_id);
                        let buffer = info
                            .trackers
                            .buffers
                            .use_extend(&*buffer_guard, buffer_id, (), hal::BufferUses::INDEX)
                            .map_err(|e| RenderCommandError::Buffer(buffer_id, e))
                            .map_pass_err(scope)?;
                        check_buffer_usage(buffer.usage, BufferUsages::INDEX)
                            .map_pass_err(scope)?;
                        let buf_raw = buffer
                            .raw
                            .as_ref()
                            .ok_or(RenderCommandError::DestroyedBuffer(buffer_id))
                            .map_pass_err(scope)?;

                        let end = match size {
                            Some(s) => offset + s.get(),
                            None => buffer.size,
                        };
                        state.index.bound_buffer_view = Some((id::Valid(buffer_id), offset..end));

                        state.index.format = Some(index_format);
                        state.index.update_limit();

                        cmd_buf.buffer_memory_init_actions.extend(
                            buffer.initialization_status.create_action(
                                buffer_id,
                                offset..end,
                                MemoryInitKind::NeedsInitializedMemory,
                            ),
                        );

                        let bb = hal::BufferBinding {
                            buffer: buf_raw,
                            offset,
                            size,
                        };
                        unsafe {
                            raw.set_index_buffer(bb, index_format);
                        }
                    }
                    RenderCommand::SetVertexBuffer {
                        slot,
                        buffer_id,
                        offset,
                        size,
                    } => {
                        let scope = PassErrorScope::SetVertexBuffer(buffer_id);
                        let buffer = info
                            .trackers
                            .buffers
                            .use_extend(&*buffer_guard, buffer_id, (), hal::BufferUses::VERTEX)
                            .map_err(|e| RenderCommandError::Buffer(buffer_id, e))
                            .map_pass_err(scope)?;
                        check_buffer_usage(buffer.usage, BufferUsages::VERTEX)
                            .map_pass_err(scope)?;
                        let buf_raw = buffer
                            .raw
                            .as_ref()
                            .ok_or(RenderCommandError::DestroyedBuffer(buffer_id))
                            .map_pass_err(scope)?;

                        let empty_slots =
                            (1 + slot as usize).saturating_sub(state.vertex.inputs.len());
                        state
                            .vertex
                            .inputs
                            .extend(iter::repeat(VertexBufferState::EMPTY).take(empty_slots));
                        let vertex_state = &mut state.vertex.inputs[slot as usize];
                        //TODO: where are we checking that the offset is in bound?
                        vertex_state.total_size = match size {
                            Some(s) => s.get(),
                            None => buffer.size - offset,
                        };
                        vertex_state.bound = true;

                        cmd_buf.buffer_memory_init_actions.extend(
                            buffer.initialization_status.create_action(
                                buffer_id,
                                offset..(offset + vertex_state.total_size),
                                MemoryInitKind::NeedsInitializedMemory,
                            ),
                        );

                        let bb = hal::BufferBinding {
                            buffer: buf_raw,
                            offset,
                            size,
                        };
                        unsafe {
                            raw.set_vertex_buffer(slot, bb);
                        }
                        state.vertex.update_limits();
                    }
                    RenderCommand::SetBlendConstant(ref color) => {
                        state.blend_constant = OptionalState::Set;
                        let array = [
                            color.r as f32,
                            color.g as f32,
                            color.b as f32,
                            color.a as f32,
                        ];
                        unsafe {
                            raw.set_blend_constants(&array);
                        }
                    }
                    RenderCommand::SetStencilReference(value) => {
                        state.stencil_reference = value;
                        if state
                            .pipeline_flags
                            .contains(PipelineFlags::STENCIL_REFERENCE)
                        {
                            unsafe {
                                raw.set_stencil_reference(value);
                            }
                        }
                    }
                    RenderCommand::SetViewport {
                        ref rect,
                        depth_min,
                        depth_max,
                    } => {
                        let scope = PassErrorScope::SetViewport;
                        if rect.w <= 0.0
                            || rect.h <= 0.0
                            || depth_min < 0.0
                            || depth_min > 1.0
                            || depth_max < 0.0
                            || depth_max > 1.0
                        {
                            return Err(RenderCommandError::InvalidViewport).map_pass_err(scope);
                        }
                        let r = hal::Rect {
                            x: rect.x,
                            y: rect.y,
                            w: rect.w,
                            h: rect.h,
                        };
                        unsafe {
                            raw.set_viewport(&r, depth_min..depth_max);
                        }
                    }
                    RenderCommand::SetPushConstant {
                        stages,
                        offset,
                        size_bytes,
                        values_offset,
                    } => {
                        let scope = PassErrorScope::SetPushConstant;
                        let values_offset = values_offset
                            .ok_or(RenderPassErrorInner::InvalidValuesOffset)
                            .map_pass_err(scope)?;

                        let end_offset_bytes = offset + size_bytes;
                        let values_end_offset =
                            (values_offset + size_bytes / wgt::PUSH_CONSTANT_ALIGNMENT) as usize;
                        let data_slice =
                            &base.push_constant_data[(values_offset as usize)..values_end_offset];

                        let pipeline_layout_id = state
                            .binder
                            .pipeline_layout_id
                            .ok_or(DrawError::MissingPipeline)
                            .map_pass_err(scope)?;
                        let pipeline_layout = &pipeline_layout_guard[pipeline_layout_id];

                        pipeline_layout
                            .validate_push_constant_ranges(stages, offset, end_offset_bytes)
                            .map_err(RenderCommandError::from)
                            .map_pass_err(scope)?;

                        unsafe {
                            raw.set_push_constants(&pipeline_layout.raw, stages, offset, data_slice)
                        }
                    }
                    RenderCommand::SetScissor(ref rect) => {
                        let scope = PassErrorScope::SetScissorRect;
                        if rect.w == 0
                            || rect.h == 0
                            || rect.x + rect.w > info.extent.width
                            || rect.y + rect.h > info.extent.height
                        {
                            return Err(RenderCommandError::InvalidScissorRect).map_pass_err(scope);
                        }
                        let r = hal::Rect {
                            x: rect.x,
                            y: rect.y,
                            w: rect.w,
                            h: rect.h,
                        };
                        unsafe {
                            raw.set_scissor_rect(&r);
                        }
                    }
                    RenderCommand::Draw {
                        vertex_count,
                        instance_count,
                        first_vertex,
                        first_instance,
                    } => {
                        let indexed = false;
                        let scope = PassErrorScope::Draw {
                            indexed,
                            indirect: false,
                            pipeline: state.pipeline.last_state,
                        };
                        state.is_ready(indexed).map_pass_err(scope)?;

                        let last_vertex = first_vertex + vertex_count;
                        let vertex_limit = state.vertex.vertex_limit;
                        if last_vertex > vertex_limit {
                            return Err(DrawError::VertexBeyondLimit {
                                last_vertex,
                                vertex_limit,
                                slot: state.vertex.vertex_limit_slot,
                            })
                            .map_pass_err(scope);
                        }
                        let last_instance = first_instance + instance_count;
                        let instance_limit = state.vertex.instance_limit;
                        if last_instance > instance_limit {
                            return Err(DrawError::InstanceBeyondLimit {
                                last_instance,
                                instance_limit,
                                slot: state.vertex.instance_limit_slot,
                            })
                            .map_pass_err(scope);
                        }

                        unsafe {
                            raw.draw(first_vertex, vertex_count, first_instance, instance_count);
                        }
                    }
                    RenderCommand::DrawIndexed {
                        index_count,
                        instance_count,
                        first_index,
                        base_vertex,
                        first_instance,
                    } => {
                        let indexed = true;
                        let scope = PassErrorScope::Draw {
                            indexed,
                            indirect: false,
                            pipeline: state.pipeline.last_state,
                        };
                        state.is_ready(indexed).map_pass_err(scope)?;

                        //TODO: validate that base_vertex + max_index() is within the provided range
                        let last_index = first_index + index_count;
                        let index_limit = state.index.limit;
                        if last_index > index_limit {
                            return Err(DrawError::IndexBeyondLimit {
                                last_index,
                                index_limit,
                            })
                            .map_pass_err(scope);
                        }
                        let last_instance = first_instance + instance_count;
                        let instance_limit = state.vertex.instance_limit;
                        if last_instance > instance_limit {
                            return Err(DrawError::InstanceBeyondLimit {
                                last_instance,
                                instance_limit,
                                slot: state.vertex.instance_limit_slot,
                            })
                            .map_pass_err(scope);
                        }

                        unsafe {
                            raw.draw_indexed(
                                first_index,
                                index_count,
                                base_vertex,
                                first_instance,
                                instance_count,
                            );
                        }
                    }
                    RenderCommand::MultiDrawIndirect {
                        buffer_id,
                        offset,
                        count,
                        indexed,
                    } => {
                        let scope = PassErrorScope::Draw {
                            indexed,
                            indirect: true,
                            pipeline: state.pipeline.last_state,
                        };
                        state.is_ready(indexed).map_pass_err(scope)?;

                        let stride = match indexed {
                            false => mem::size_of::<wgt::DrawIndirectArgs>(),
                            true => mem::size_of::<wgt::DrawIndexedIndirectArgs>(),
                        };

                        if count.is_some() {
                            device
                                .require_features(wgt::Features::MULTI_DRAW_INDIRECT)
                                .map_pass_err(scope)?;
                        }
                        device
                            .require_downlevel_flags(wgt::DownlevelFlags::INDIRECT_EXECUTION)
                            .map_pass_err(scope)?;

                        let indirect_buffer = info
                            .trackers
                            .buffers
                            .use_extend(&*buffer_guard, buffer_id, (), hal::BufferUses::INDIRECT)
                            .map_err(|e| RenderCommandError::Buffer(buffer_id, e))
                            .map_pass_err(scope)?;
                        check_buffer_usage(indirect_buffer.usage, BufferUsages::INDIRECT)
                            .map_pass_err(scope)?;
                        let indirect_raw = indirect_buffer
                            .raw
                            .as_ref()
                            .ok_or(RenderCommandError::DestroyedBuffer(buffer_id))
                            .map_pass_err(scope)?;

                        let actual_count = count.map_or(1, |c| c.get());

                        let end_offset = offset + stride as u64 * actual_count as u64;
                        if end_offset > indirect_buffer.size {
                            return Err(RenderPassErrorInner::IndirectBufferOverrun {
                                count,
                                offset,
                                end_offset,
                                buffer_size: indirect_buffer.size,
                            })
                            .map_pass_err(scope);
                        }

                        cmd_buf.buffer_memory_init_actions.extend(
                            indirect_buffer.initialization_status.create_action(
                                buffer_id,
                                offset..end_offset,
                                MemoryInitKind::NeedsInitializedMemory,
                            ),
                        );

                        match indexed {
                            false => unsafe {
                                raw.draw_indirect(indirect_raw, offset, actual_count);
                            },
                            true => unsafe {
                                raw.draw_indexed_indirect(indirect_raw, offset, actual_count);
                            },
                        }
                    }
                    RenderCommand::MultiDrawIndirectCount {
                        buffer_id,
                        offset,
                        count_buffer_id,
                        count_buffer_offset,
                        max_count,
                        indexed,
                    } => {
                        let scope = PassErrorScope::Draw {
                            indexed,
                            indirect: true,
                            pipeline: state.pipeline.last_state,
                        };
                        state.is_ready(indexed).map_pass_err(scope)?;

                        let stride = match indexed {
                            false => mem::size_of::<wgt::DrawIndirectArgs>(),
                            true => mem::size_of::<wgt::DrawIndexedIndirectArgs>(),
                        } as u64;

                        device
                            .require_features(wgt::Features::MULTI_DRAW_INDIRECT_COUNT)
                            .map_pass_err(scope)?;
                        device
                            .require_downlevel_flags(wgt::DownlevelFlags::INDIRECT_EXECUTION)
                            .map_pass_err(scope)?;

                        let indirect_buffer = info
                            .trackers
                            .buffers
                            .use_extend(&*buffer_guard, buffer_id, (), hal::BufferUses::INDIRECT)
                            .map_err(|e| RenderCommandError::Buffer(buffer_id, e))
                            .map_pass_err(scope)?;
                        check_buffer_usage(indirect_buffer.usage, BufferUsages::INDIRECT)
                            .map_pass_err(scope)?;
                        let indirect_raw = indirect_buffer
                            .raw
                            .as_ref()
                            .ok_or(RenderCommandError::DestroyedBuffer(buffer_id))
                            .map_pass_err(scope)?;

                        let count_buffer = info
                            .trackers
                            .buffers
                            .use_extend(
                                &*buffer_guard,
                                count_buffer_id,
                                (),
                                hal::BufferUses::INDIRECT,
                            )
                            .map_err(|e| RenderCommandError::Buffer(count_buffer_id, e))
                            .map_pass_err(scope)?;
                        check_buffer_usage(count_buffer.usage, BufferUsages::INDIRECT)
                            .map_pass_err(scope)?;
                        let count_raw = count_buffer
                            .raw
                            .as_ref()
                            .ok_or(RenderCommandError::DestroyedBuffer(count_buffer_id))
                            .map_pass_err(scope)?;

                        let end_offset = offset + stride * max_count as u64;
                        if end_offset > indirect_buffer.size {
                            return Err(RenderPassErrorInner::IndirectBufferOverrun {
                                count: None,
                                offset,
                                end_offset,
                                buffer_size: indirect_buffer.size,
                            })
                            .map_pass_err(scope);
                        }
                        cmd_buf.buffer_memory_init_actions.extend(
                            indirect_buffer.initialization_status.create_action(
                                buffer_id,
                                offset..end_offset,
                                MemoryInitKind::NeedsInitializedMemory,
                            ),
                        );

                        let begin_count_offset = count_buffer_offset;
                        let end_count_offset = count_buffer_offset + 4;
                        if end_count_offset > count_buffer.size {
                            return Err(RenderPassErrorInner::IndirectCountBufferOverrun {
                                begin_count_offset,
                                end_count_offset,
                                count_buffer_size: count_buffer.size,
                            })
                            .map_pass_err(scope);
                        }
                        cmd_buf.buffer_memory_init_actions.extend(
                            count_buffer.initialization_status.create_action(
                                count_buffer_id,
                                count_buffer_offset..end_count_offset,
                                MemoryInitKind::NeedsInitializedMemory,
                            ),
                        );

                        match indexed {
                            false => unsafe {
                                raw.draw_indirect_count(
                                    indirect_raw,
                                    offset,
                                    count_raw,
                                    count_buffer_offset,
                                    max_count,
                                );
                            },
                            true => unsafe {
                                raw.draw_indexed_indirect_count(
                                    indirect_raw,
                                    offset,
                                    count_raw,
                                    count_buffer_offset,
                                    max_count,
                                );
                            },
                        }
                    }
                    RenderCommand::PushDebugGroup { color: _, len } => {
                        state.debug_scope_depth += 1;
                        let label =
                            str::from_utf8(&base.string_data[string_offset..string_offset + len])
                                .unwrap();
                        string_offset += len;
                        unsafe {
                            raw.begin_debug_marker(label);
                        }
                    }
                    RenderCommand::PopDebugGroup => {
                        let scope = PassErrorScope::PopDebugGroup;
                        if state.debug_scope_depth == 0 {
                            return Err(RenderPassErrorInner::InvalidPopDebugGroup)
                                .map_pass_err(scope);
                        }
                        state.debug_scope_depth -= 1;
                        unsafe {
                            raw.end_debug_marker();
                        }
                    }
                    RenderCommand::InsertDebugMarker { color: _, len } => {
                        let label =
                            str::from_utf8(&base.string_data[string_offset..string_offset + len])
                                .unwrap();
                        string_offset += len;
                        unsafe {
                            raw.insert_debug_marker(label);
                        }
                    }
                    RenderCommand::WriteTimestamp {
                        query_set_id,
                        query_index,
                    } => {
                        let scope = PassErrorScope::WriteTimestamp;

                        let query_set = cmd_buf
                            .trackers
                            .query_sets
                            .use_extend(&*query_set_guard, query_set_id, (), ())
                            .map_err(|e| match e {
                                UseExtendError::InvalidResource => {
                                    RenderCommandError::InvalidQuerySet(query_set_id)
                                }
                                _ => unreachable!(),
                            })
                            .map_pass_err(scope)?;

                        query_set
                            .validate_and_write_timestamp(
                                raw,
                                query_set_id,
                                query_index,
                                Some(&mut query_reset_state),
                            )
                            .map_pass_err(scope)?;
                    }
                    RenderCommand::BeginPipelineStatisticsQuery {
                        query_set_id,
                        query_index,
                    } => {
                        let scope = PassErrorScope::BeginPipelineStatisticsQuery;

                        let query_set = cmd_buf
                            .trackers
                            .query_sets
                            .use_extend(&*query_set_guard, query_set_id, (), ())
                            .map_err(|e| match e {
                                UseExtendError::InvalidResource => {
                                    RenderCommandError::InvalidQuerySet(query_set_id)
                                }
                                _ => unreachable!(),
                            })
                            .map_pass_err(scope)?;

                        query_set
                            .validate_and_begin_pipeline_statistics_query(
                                raw,
                                query_set_id,
                                query_index,
                                Some(&mut query_reset_state),
                                &mut active_query,
                            )
                            .map_pass_err(scope)?;
                    }
                    RenderCommand::EndPipelineStatisticsQuery => {
                        let scope = PassErrorScope::EndPipelineStatisticsQuery;

                        end_pipeline_statistics_query(raw, &*query_set_guard, &mut active_query)
                            .map_pass_err(scope)?;
                    }
                    RenderCommand::ExecuteBundle(bundle_id) => {
                        let scope = PassErrorScope::ExecuteBundle;
                        let bundle = cmd_buf
                            .trackers
                            .bundles
                            .use_extend(&*bundle_guard, bundle_id, (), ())
                            .map_err(|_| RenderCommandError::InvalidRenderBundle(bundle_id))
                            .map_pass_err(scope)?;

                        info.context
                            .check_compatible(&bundle.context)
                            .map_err(RenderPassErrorInner::IncompatibleBundleTargets)
                            .map_pass_err(scope)?;

                        if info.is_ds_read_only != bundle.is_ds_read_only {
                            return Err(RenderPassErrorInner::IncompatibleBundleRods {
                                pass: info.is_ds_read_only,
                                bundle: bundle.is_ds_read_only,
                            })
                            .map_pass_err(scope);
                        }

                        cmd_buf.buffer_memory_init_actions.extend(
                            bundle
                                .buffer_memory_init_actions
                                .iter()
                                .filter_map(|action| match buffer_guard.get(action.id) {
                                    Ok(buffer) => buffer.initialization_status.check_action(action),
                                    Err(_) => None,
                                }),
                        );
                        for action in bundle.texture_memory_init_actions.iter() {
                            info.pending_discard_init_fixups.extend(
                                cmd_buf
                                    .texture_memory_actions
                                    .register_init_action(action, &texture_guard),
                            );
                        }

                        unsafe {
                            bundle.execute(
                                raw,
                                &*pipeline_layout_guard,
                                &*bind_group_guard,
                                &*pipeline_guard,
                                &*buffer_guard,
                            )
                        }
                        .map_err(|e| match e {
                            ExecutionError::DestroyedBuffer(id) => {
                                RenderCommandError::DestroyedBuffer(id)
                            }
                            ExecutionError::Unimplemented(what) => {
                                RenderCommandError::Unimplemented(what)
                            }
                        })
                        .map_pass_err(scope)?;

                        info.trackers
                            .merge_extend(&bundle.used)
                            .map_pass_err(scope)?;
                        // Start tracking the bind groups specifically, as they are the only
                        // compound resources, to make it easier to update submission indices
                        // later at submission time.
                        cmd_buf
                            .trackers
                            .bind_groups
                            .merge_extend(&bundle.used.bind_groups)
                            .unwrap();
                        state.reset_bundle();
                    }
                }
            }

            log::trace!("Merging {:?} with the render pass", encoder_id);
            let (trackers, pending_discard_init_fixups) =
                info.finish(raw, &*texture_guard).map_pass_err(init_scope)?;

            cmd_buf.encoder.close();
            (trackers, query_reset_state, pending_discard_init_fixups)
        };

        let (mut cmb_guard, mut token) = hub.command_buffers.write(&mut token);
        let (query_set_guard, mut token) = hub.query_sets.read(&mut token);
        let (buffer_guard, mut token) = hub.buffers.read(&mut token);
        let (texture_guard, _) = hub.textures.read(&mut token);

        let cmd_buf = cmb_guard.get_mut(encoder_id).unwrap();
        {
            let transit = cmd_buf.encoder.open();

            fixup_discarded_surfaces(
                pending_discard_init_fixups.into_iter(),
                transit,
                &texture_guard,
                &mut cmd_buf.trackers.textures,
                &device_guard[cmd_buf.device_id.value],
            );

            query_reset_state
                .reset_queries(
                    transit,
                    &query_set_guard,
                    cmd_buf.device_id.value.0.backend(),
                )
                .map_err(RenderCommandError::InvalidQuerySet)
                .map_pass_err(PassErrorScope::QueryReset)?;

            super::CommandBuffer::insert_barriers(
                transit,
                &mut cmd_buf.trackers,
                &trackers.buffers,
                &trackers.textures,
                &*buffer_guard,
                &*texture_guard,
            );
        }

        // Before we finish the auxiliary encoder, let's
        // get our pass back and place it after.
        //Note: we could just hold onto this raw pass while recording the
        // auxiliary encoder, but then handling errors and cleaning up
        // would be more complicated, so we re-use `open()`/`close()`.
        let pass_raw = cmd_buf.encoder.list.pop().unwrap();
        cmd_buf.encoder.close();
        cmd_buf.encoder.list.push(pass_raw);
        cmd_buf.status = CommandEncoderStatus::Recording;

        Ok(())
    }
}

pub mod render_ffi {
    use super::{
        super::{Rect, RenderCommand},
        RenderPass,
    };
    use crate::{id, RawString};
    use std::{convert::TryInto, ffi, num::NonZeroU32, slice};
    use wgt::{BufferAddress, BufferSize, Color, DynamicOffset};

    /// # Safety
    ///
    /// This function is unsafe as there is no guarantee that the given pointer is
    /// valid for `offset_length` elements.
    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_set_bind_group(
        pass: &mut RenderPass,
        index: u32,
        bind_group_id: id::BindGroupId,
        offsets: *const DynamicOffset,
        offset_length: usize,
    ) {
        pass.base.commands.push(RenderCommand::SetBindGroup {
            index: index.try_into().unwrap(),
            num_dynamic_offsets: offset_length.try_into().unwrap(),
            bind_group_id,
        });
        if offset_length != 0 {
            pass.base
                .dynamic_offsets
                .extend_from_slice(slice::from_raw_parts(offsets, offset_length));
        }
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_set_pipeline(
        pass: &mut RenderPass,
        pipeline_id: id::RenderPipelineId,
    ) {
        pass.base
            .commands
            .push(RenderCommand::SetPipeline(pipeline_id));
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_set_vertex_buffer(
        pass: &mut RenderPass,
        slot: u32,
        buffer_id: id::BufferId,
        offset: BufferAddress,
        size: Option<BufferSize>,
    ) {
        pass.base.commands.push(RenderCommand::SetVertexBuffer {
            slot,
            buffer_id,
            offset,
            size,
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_set_blend_constant(pass: &mut RenderPass, color: &Color) {
        pass.base
            .commands
            .push(RenderCommand::SetBlendConstant(*color));
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_set_stencil_reference(pass: &mut RenderPass, value: u32) {
        pass.base
            .commands
            .push(RenderCommand::SetStencilReference(value));
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_set_viewport(
        pass: &mut RenderPass,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        depth_min: f32,
        depth_max: f32,
    ) {
        pass.base.commands.push(RenderCommand::SetViewport {
            rect: Rect { x, y, w, h },
            depth_min,
            depth_max,
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_set_scissor_rect(
        pass: &mut RenderPass,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) {
        pass.base
            .commands
            .push(RenderCommand::SetScissor(Rect { x, y, w, h }));
    }

    /// # Safety
    ///
    /// This function is unsafe as there is no guarantee that the given pointer is
    /// valid for `size_bytes` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_set_push_constants(
        pass: &mut RenderPass,
        stages: wgt::ShaderStages,
        offset: u32,
        size_bytes: u32,
        data: *const u8,
    ) {
        assert_eq!(
            offset & (wgt::PUSH_CONSTANT_ALIGNMENT - 1),
            0,
            "Push constant offset must be aligned to 4 bytes."
        );
        assert_eq!(
            size_bytes & (wgt::PUSH_CONSTANT_ALIGNMENT - 1),
            0,
            "Push constant size must be aligned to 4 bytes."
        );
        let data_slice = slice::from_raw_parts(data, size_bytes as usize);
        let value_offset = pass.base.push_constant_data.len().try_into().expect(
            "Ran out of push constant space. Don't set 4gb of push constants per RenderPass.",
        );

        pass.base.push_constant_data.extend(
            data_slice
                .chunks_exact(wgt::PUSH_CONSTANT_ALIGNMENT as usize)
                .map(|arr| u32::from_ne_bytes([arr[0], arr[1], arr[2], arr[3]])),
        );

        pass.base.commands.push(RenderCommand::SetPushConstant {
            stages,
            offset,
            size_bytes,
            values_offset: Some(value_offset),
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_draw(
        pass: &mut RenderPass,
        vertex_count: u32,
        instance_count: u32,
        first_vertex: u32,
        first_instance: u32,
    ) {
        pass.base.commands.push(RenderCommand::Draw {
            vertex_count,
            instance_count,
            first_vertex,
            first_instance,
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_draw_indexed(
        pass: &mut RenderPass,
        index_count: u32,
        instance_count: u32,
        first_index: u32,
        base_vertex: i32,
        first_instance: u32,
    ) {
        pass.base.commands.push(RenderCommand::DrawIndexed {
            index_count,
            instance_count,
            first_index,
            base_vertex,
            first_instance,
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_draw_indirect(
        pass: &mut RenderPass,
        buffer_id: id::BufferId,
        offset: BufferAddress,
    ) {
        pass.base.commands.push(RenderCommand::MultiDrawIndirect {
            buffer_id,
            offset,
            count: None,
            indexed: false,
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_draw_indexed_indirect(
        pass: &mut RenderPass,
        buffer_id: id::BufferId,
        offset: BufferAddress,
    ) {
        pass.base.commands.push(RenderCommand::MultiDrawIndirect {
            buffer_id,
            offset,
            count: None,
            indexed: true,
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_multi_draw_indirect(
        pass: &mut RenderPass,
        buffer_id: id::BufferId,
        offset: BufferAddress,
        count: u32,
    ) {
        pass.base.commands.push(RenderCommand::MultiDrawIndirect {
            buffer_id,
            offset,
            count: NonZeroU32::new(count),
            indexed: false,
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_multi_draw_indexed_indirect(
        pass: &mut RenderPass,
        buffer_id: id::BufferId,
        offset: BufferAddress,
        count: u32,
    ) {
        pass.base.commands.push(RenderCommand::MultiDrawIndirect {
            buffer_id,
            offset,
            count: NonZeroU32::new(count),
            indexed: true,
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_multi_draw_indirect_count(
        pass: &mut RenderPass,
        buffer_id: id::BufferId,
        offset: BufferAddress,
        count_buffer_id: id::BufferId,
        count_buffer_offset: BufferAddress,
        max_count: u32,
    ) {
        pass.base
            .commands
            .push(RenderCommand::MultiDrawIndirectCount {
                buffer_id,
                offset,
                count_buffer_id,
                count_buffer_offset,
                max_count,
                indexed: false,
            });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_multi_draw_indexed_indirect_count(
        pass: &mut RenderPass,
        buffer_id: id::BufferId,
        offset: BufferAddress,
        count_buffer_id: id::BufferId,
        count_buffer_offset: BufferAddress,
        max_count: u32,
    ) {
        pass.base
            .commands
            .push(RenderCommand::MultiDrawIndirectCount {
                buffer_id,
                offset,
                count_buffer_id,
                count_buffer_offset,
                max_count,
                indexed: true,
            });
    }

    /// # Safety
    ///
    /// This function is unsafe as there is no guarantee that the given `label`
    /// is a valid null-terminated string.
    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_push_debug_group(
        pass: &mut RenderPass,
        label: RawString,
        color: u32,
    ) {
        let bytes = ffi::CStr::from_ptr(label).to_bytes();
        pass.base.string_data.extend_from_slice(bytes);

        pass.base.commands.push(RenderCommand::PushDebugGroup {
            color,
            len: bytes.len(),
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_pop_debug_group(pass: &mut RenderPass) {
        pass.base.commands.push(RenderCommand::PopDebugGroup);
    }

    /// # Safety
    ///
    /// This function is unsafe as there is no guarantee that the given `label`
    /// is a valid null-terminated string.
    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_insert_debug_marker(
        pass: &mut RenderPass,
        label: RawString,
        color: u32,
    ) {
        let bytes = ffi::CStr::from_ptr(label).to_bytes();
        pass.base.string_data.extend_from_slice(bytes);

        pass.base.commands.push(RenderCommand::InsertDebugMarker {
            color,
            len: bytes.len(),
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_write_timestamp(
        pass: &mut RenderPass,
        query_set_id: id::QuerySetId,
        query_index: u32,
    ) {
        pass.base.commands.push(RenderCommand::WriteTimestamp {
            query_set_id,
            query_index,
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_begin_pipeline_statistics_query(
        pass: &mut RenderPass,
        query_set_id: id::QuerySetId,
        query_index: u32,
    ) {
        pass.base
            .commands
            .push(RenderCommand::BeginPipelineStatisticsQuery {
                query_set_id,
                query_index,
            });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_render_pass_end_pipeline_statistics_query(pass: &mut RenderPass) {
        pass.base
            .commands
            .push(RenderCommand::EndPipelineStatisticsQuery);
    }

    /// # Safety
    ///
    /// This function is unsafe as there is no guarantee that the given pointer is
    /// valid for `render_bundle_ids_length` elements.
    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_execute_bundles(
        pass: &mut RenderPass,
        render_bundle_ids: *const id::RenderBundleId,
        render_bundle_ids_length: usize,
    ) {
        for &bundle_id in slice::from_raw_parts(render_bundle_ids, render_bundle_ids_length) {
            pass.base
                .commands
                .push(RenderCommand::ExecuteBundle(bundle_id));
        }
    }
}
