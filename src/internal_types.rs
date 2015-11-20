use app_units::Au;
use batch::{VertexBuffer, Batch, VertexBufferId, TileParams};
use device::{TextureId, TextureIndex, TextureFilter};
use euclid::{Matrix4, Point2D, Rect, Size2D};
use fnv::FnvHasher;
use freelist::{FreeListItem, FreeListItemId};
use gleam::gl;
use std::collections::HashMap;
use std::collections::hash_state::DefaultState;
use std::sync::Arc;
use texture_cache::TextureCacheItem;
use webrender_traits::{FontKey, Epoch, ColorF, PipelineId};
use webrender_traits::{ImageFormat};
use webrender_traits::{ComplexClipRegion, MixBlendMode, NativeFontHandle, DisplayItem};
use util;

const UV_FLOAT_TO_FIXED: f32 = 65535.0;
const COLOR_FLOAT_TO_FIXED: f32 = 255.0;
pub const ANGLE_FLOAT_TO_FIXED: f32 = 65535.0;

pub const ORTHO_NEAR_PLANE: f32 = -1000000.0;
pub const ORTHO_FAR_PLANE: f32 = 1000000.0;

static ZERO_RECT_F32: Rect<f32> = Rect {
    origin: Point2D {
        x: 0.0,
        y: 0.0,
    },
    size: Size2D {
        width: 0.0,
        height: 0.0,
    },
};

pub enum FontTemplate {
    Raw(Arc<Vec<u8>>),
    Native(NativeFontHandle),
}

pub type DrawListId = FreeListItemId;

#[derive(Debug, PartialEq, Eq)]
pub enum TextureSampler {
    Color,
    Mask,
}

pub enum VertexAttribute {
    Position,
    Color,
    ColorTexCoord,
    MaskTexCoord,
    BorderRadii,
    BorderPosition,
    BlurRadius,
    DestTextureSize,
    SourceTextureSize,
    Misc,
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct PackedColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl PackedColor {
    pub fn from_color(color: &ColorF) -> PackedColor {
        PackedColor {
            r: (0.5 + color.r * COLOR_FLOAT_TO_FIXED).floor() as u8,
            g: (0.5 + color.g * COLOR_FLOAT_TO_FIXED).floor() as u8,
            b: (0.5 + color.b * COLOR_FLOAT_TO_FIXED).floor() as u8,
            a: (0.5 + color.a * COLOR_FLOAT_TO_FIXED).floor() as u8,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WorkVertex {
    pub x: f32,
    pub y: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
    pub u: f32,
    pub v: f32,
}

impl WorkVertex {
    #[inline]
    pub fn new(x: f32, y: f32, color: &ColorF, u: f32, v: f32) -> WorkVertex {
        debug_assert!(u.is_finite());
        debug_assert!(v.is_finite());

        WorkVertex {
            x: x,
            y: y,
            r: color.r,
            g: color.g,
            b: color.b,
            a: color.a,
            u: u,
            v: v,
        }
    }

    pub fn position(&self) -> Point2D<f32> {
        Point2D::new(self.x, self.y)
    }

    pub fn uv(&self) -> Point2D<f32> {
        Point2D::new(self.u, self.v)
    }

    pub fn color(&self) -> ColorF {
        ColorF {
            r: self.r,
            g: self.g,
            b: self.b,
            a: self.a,
        }
    }
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct PackedVertex {
    pub x: f32,
    pub y: f32,
    pub color: PackedColor,
    pub u: f32,
    pub v: f32,
    pub mu: u16,
    pub mv: u16,
    pub matrix_index: u8,
    pub uv_index: u8,
    pub muv_index: u8,
    pub tile_params_index: u8,
}

impl PackedVertex {
    pub fn from_components(x: f32,
                           y: f32,
                           color: &ColorF,
                           u: f32,
                           v: f32,
                           mu: f32,
                           mv: f32,
                           uv_index: TextureIndex,
                           muv_index: TextureIndex)
                           -> PackedVertex {
        PackedVertex {
            x: x,
            y: y,
            color: PackedColor::from_color(color),
            u: u,
            v: v,
            mu: (mu * UV_FLOAT_TO_FIXED) as u16,
            mv: (mv * UV_FLOAT_TO_FIXED) as u16,
            matrix_index: 0,
            uv_index: uv_index.0,
            muv_index: muv_index.0,
            tile_params_index: 0,
        }
    }

    /// Just like the above function, but doesn't scale the mask uv coordinates. This is useful
    /// for the filter fragment shader, which uses the mask uv coordinates to store the texture
    /// size.
    pub fn from_components_unscaled_muv(x: f32, y: f32,
                                        color: &ColorF,
                                        u: f32, v: f32,
                                        mu: u16, mv: u16,
                                        uv_index: TextureIndex,
                                        muv_index: TextureIndex)
                                        -> PackedVertex {
        PackedVertex {
            x: x,
            y: y,
            color: PackedColor::from_color(color),
            u: u,
            v: v,
            mu: mu,
            mv: mv,
            matrix_index: 0,
            uv_index: uv_index.0,
            muv_index: muv_index.0,
            tile_params_index: 0,
        }
    }

    pub fn from_points(position: &Point2D<f32>,
                       color: &ColorF,
                       uv: &Point2D<f32>,
                       muv: &Point2D<f32>,
                       uv_index: TextureIndex,
                       muv_index: TextureIndex)
                       -> PackedVertex {
        PackedVertex::from_components(position.x, position.y,
                                      color,
                                      uv.x, uv.y,
                                      muv.x, muv.y,
                                      uv_index,
                                      muv_index)
    }
}

#[derive(Debug)]
pub enum RenderTargetMode {
    None,
    RenderTarget,
}

#[derive(Debug)]
pub enum TextureUpdateDetails {
    Blit(Vec<u8>),
    Blur(Vec<u8>, Size2D<u32>, Au, TextureImage, TextureImage),
    /// All four corners, the tessellation index, and whether inverted, respectively.
    BorderRadius(Au, Au, Au, Au, u32, bool),
    /// Blur radius, box shadow part, and whether inverted, respectively.
    BoxShadow(Au, BoxShadowPart, bool),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BoxShadowPart {
    /// The edge.
    Edge,

    /// A corner with a border radius.
    ///
    /// TODO(pcwalton): Elliptical radii.
    Corner(Au),
}

#[derive(Clone, Copy, Debug)]
pub struct TextureImage {
    pub texture_id: TextureId,
    pub texture_index: TextureIndex,
    pub texel_uv: Rect<f32>,
    pub pixel_uv: Point2D<u32>,
}

pub enum TextureUpdateOp {
    Create(TextureTarget, u32, u32, u32, ImageFormat, TextureFilter, RenderTargetMode, Option<Vec<u8>>),
    Update(u32, u32, u32, u32, TextureUpdateDetails),
    DeinitRenderTarget(TextureId),
}

pub struct TextureUpdate {
    pub id: TextureId,
    pub index: TextureIndex,
    pub op: TextureUpdateOp,
}

pub struct TextureUpdateList {
    pub updates: Vec<TextureUpdate>,
}

impl TextureUpdateList {
    pub fn new() -> TextureUpdateList {
        TextureUpdateList {
            updates: Vec::new(),
        }
    }

    #[inline]
    pub fn push(&mut self, update: TextureUpdate) {
        self.updates.push(update);
    }
}

pub enum BatchUpdateOp {
    Create(Vec<PackedVertex>, Vec<u16>),
    Destroy,
}

pub struct BatchUpdate {
    pub id: VertexBufferId,
    pub op: BatchUpdateOp,
}

pub struct BatchUpdateList {
    pub updates: Vec<BatchUpdate>,
}

impl BatchUpdateList {
    pub fn new() -> BatchUpdateList {
        BatchUpdateList {
            updates: Vec::new(),
        }
    }

    #[inline]
    pub fn push(&mut self, update: BatchUpdate) {
        self.updates.push(update);
    }
}

// TODO(gw): Use bitflags crate for ClearInfo...
// TODO(gw): Expand clear info to handle color, depth etc as needed.

#[derive(Clone, Debug)]
pub struct ClearInfo {
    pub clear_color: bool,
    pub clear_z: bool,
    pub clear_stencil: bool,
}

#[derive(Clone, Debug)]
pub struct CompositeInfo {
    pub operation: CompositionOp,
    pub rect: Rect<u32>,
    pub color_texture_id: TextureId,
}

#[derive(Clone, Debug)]
pub struct DrawCall {
    pub tile_params: Vec<TileParams>,
    pub vertex_buffer_id: VertexBufferId,
    pub color_texture_id: TextureId,
    pub mask_texture_id: TextureId,
    pub first_vertex: u16,
    pub index_count: u16,
}

#[derive(Clone, Debug)]
pub struct BatchInfo {
    pub matrix_palette: Vec<Matrix4>,
    pub draw_calls: Vec<DrawCall>,
}

impl BatchInfo {
    pub fn new(matrix_palette: Vec<Matrix4>) -> BatchInfo {
        BatchInfo {
            matrix_palette: matrix_palette,
            draw_calls: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum DrawCommand {
    Batch(BatchInfo),
    Composite(CompositeInfo),
    Clear(ClearInfo),
}

#[derive(Clone, Copy, Debug, Ord, PartialOrd, PartialEq, Eq, Hash)]
pub struct RenderTargetIndex(pub u32);

#[derive(Debug)]
pub struct DrawLayer {
    pub texture_id: Option<TextureId>,
    pub size: Size2D<u32>,
    pub commands: Vec<DrawCommand>,
}

impl DrawLayer {
    pub fn new(texture_id: Option<TextureId>,
               size: Size2D<u32>,
               commands: Vec<DrawCommand>) -> DrawLayer {
        DrawLayer {
            texture_id: texture_id,
            size: size,
            commands: commands,
        }
    }
}

pub struct RendererFrame {
    pub pipeline_epoch_map: HashMap<PipelineId, Epoch, DefaultState<FnvHasher>>,
    pub layers: Vec<DrawLayer>,
}

impl RendererFrame {
    pub fn new(pipeline_epoch_map: HashMap<PipelineId, Epoch, DefaultState<FnvHasher>>) -> RendererFrame {
        RendererFrame {
            pipeline_epoch_map: pipeline_epoch_map,
            layers: Vec::new(),
        }
    }
}

pub enum ResultMsg {
    UpdateTextureCache(TextureUpdateList),
    UpdateBatches(BatchUpdateList),
    NewFrame(RendererFrame),
}

#[derive(Debug, Clone, Copy)]
pub struct ClipRectToRegionMaskResult {
    /// The bounding box of the mask, in texture coordinates.
    pub muv_rect: Rect<f32>,

    /// The bounding rect onto which the mask will be applied, in framebuffer coordinates.
    pub position_rect: Rect<f32>,

    /// The border radius in question, for lookup in the texture cache.
    pub border_radius: f32,
}

impl ClipRectToRegionMaskResult {
    pub fn new(muv_rect: &Rect<f32>, position_rect: &Rect<f32>, border_radius: f32)
               -> ClipRectToRegionMaskResult {
        ClipRectToRegionMaskResult {
            muv_rect: *muv_rect,
            position_rect: *position_rect,
            border_radius: border_radius,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ClipRectToRegionResult<P> {
    pub rect_result: P,
    pub mask_result: Option<ClipRectToRegionMaskResult>,
}

impl<P> ClipRectToRegionResult<P> {
    pub fn new(rect_result: P, mask_result: Option<ClipRectToRegionMaskResult>)
               -> ClipRectToRegionResult<P> {
        ClipRectToRegionResult {
            rect_result: rect_result,
            mask_result: mask_result,
        }
    }

    pub fn muv_for_position(&self, position: &Point2D<f32>, mask: &TextureCacheItem)
                            -> Point2D<f32> {
        let mask_uv_size = Size2D::new(mask.uv_rect.bottom_right.x - mask.uv_rect.top_left.x,
                                       mask.uv_rect.bottom_right.y - mask.uv_rect.top_left.y);
        let mask_result = match self.mask_result {
            None => return Point2D::new(0.0, 0.0),
            Some(ref mask_result) => mask_result,
        };

        let muv_rect =
            Rect::new(Point2D::new(mask.uv_rect.top_left.x + mask_result.muv_rect.origin.x * mask_uv_size.width,
                                   mask.uv_rect.top_left.y + mask_result.muv_rect.origin.y * mask_uv_size.height),
                      Size2D::new(mask_result.muv_rect.size.width * mask_uv_size.width,
                                  mask_result.muv_rect.size.height * mask_uv_size.height));
        let position_rect = &mask_result.position_rect;

        Point2D::new(util::lerp(muv_rect.origin.x,
                                muv_rect.max_x(),
                                (position.x - position_rect.origin.x) / position_rect.size.width),
                     util::lerp(muv_rect.origin.y,
                                muv_rect.max_y(),
                                (position.y - position_rect.origin.y) / position_rect.size.height))
    }

    pub fn make_packed_vertex(&self,
                              position: &Point2D<f32>,
                              uv: &Point2D<f32>,
                              color: &ColorF,
                              mask: &TextureCacheItem,
                              uv_index: TextureIndex)
                              -> PackedVertex {
        PackedVertex::from_points(position,
                                  color,
                                  uv,
                                  &self.muv_for_position(position, mask),
                                  uv_index,
                                  mask.texture_index)
    }
}

impl ClipRectToRegionResult<RectPosUv> {
    // TODO(pcwalton): Clip colors too!
    pub fn make_packed_vertices_for_rect(&self,
                                         colors: &[ColorF; 4],
                                         mask: &TextureCacheItem,
                                         uv_index: TextureIndex)
                                         -> [PackedVertex; 4] {
        [
            self.make_packed_vertex(&self.rect_result.pos.origin,
                                    &self.rect_result.uv.top_left,
                                    &colors[0],
                                    mask,
                                    uv_index),
            self.make_packed_vertex(&self.rect_result.pos.top_right(),
                                    &self.rect_result.uv.top_right,
                                    &colors[1],
                                    mask,
                                    uv_index),
            self.make_packed_vertex(&self.rect_result.pos.bottom_left(),
                                    &self.rect_result.uv.bottom_left,
                                    &colors[3],
                                    mask,
                                    uv_index),
            self.make_packed_vertex(&self.rect_result.pos.bottom_right(),
                                    &self.rect_result.uv.bottom_right,
                                    &colors[2],
                                    mask,
                                    uv_index),
        ]
    }
}

#[derive(Debug, Clone, Copy)]
pub enum BorderEdgeDirection {
    Horizontal,
    Vertical,
}

#[derive(Debug)]
pub struct DrawListContext {
    pub origin: Point2D<f32>,
    pub overflow: Rect<f32>,
    pub final_transform: Matrix4,
}

#[derive(Debug)]
pub struct DrawList {
    pub items: Vec<DisplayItem>,

    pub context: Option<DrawListContext>,

    // TODO(gw): Structure squat to remove this field.
    next_free_id: Option<FreeListItemId>,
}

impl DrawList {
    pub fn new(items: Vec<DisplayItem>) -> DrawList {
        DrawList {
            items: items,
            context: None,
            next_free_id: None,
        }
    }
}

impl FreeListItem for DrawList {
    fn next_free_id(&self) -> Option<FreeListItemId> {
        self.next_free_id
    }

    fn set_next_free_id(&mut self, id: Option<FreeListItemId>) {
        self.next_free_id = id;
    }
}

#[derive(Clone, Copy, Debug, Ord, PartialOrd, PartialEq, Eq)]
pub struct DrawListItemIndex(pub u32);

#[derive(Debug, Copy, Clone)]
pub enum Primitive {
    Triangles,
    Rectangles,     // 4 vertices per rect
    TriangleFan,    // simple triangle fan (typically from clipper)
    Glyphs,         // font glyphs (some platforms may specialize shader)
}

pub struct BatchList {
    pub batches: Vec<Batch>,
    pub first_draw_list_id: DrawListId,
}

pub struct CompiledNode {
    // TODO(gw): These are mutually exclusive - unify into an enum?
    pub vertex_buffer: Option<VertexBuffer>,
    pub vertex_buffer_id: Option<VertexBufferId>,

    pub batch_list: Vec<BatchList>,
}

impl CompiledNode {
    pub fn new() -> CompiledNode {
        CompiledNode {
            batch_list: Vec::new(),
            vertex_buffer: None,
            vertex_buffer_id: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RectPosUv {
    pub pos: Rect<f32>,
    pub uv: RectUv,
}

#[derive(Clone, Copy, Debug)]
pub struct RectUv {
    pub top_left: Point2D<f32>,
    pub top_right: Point2D<f32>,
    pub bottom_left: Point2D<f32>,
    pub bottom_right: Point2D<f32>,
}

impl RectUv {
    pub fn from_image_and_rotation_angle(image: &TextureCacheItem,
                                         rotation_angle: BasicRotationAngle,
                                         flip_90_degree_rotations: bool)
                                         -> RectUv {
        match (rotation_angle, flip_90_degree_rotations) {
            (BasicRotationAngle::Upright, _) => {
                RectUv {
                    top_left: image.uv_rect.top_left,
                    top_right: image.uv_rect.top_right,
                    bottom_right: image.uv_rect.bottom_right,
                    bottom_left: image.uv_rect.bottom_left,
                }
            }
            (BasicRotationAngle::Clockwise90, true) => {
                RectUv {
                    top_right: image.uv_rect.top_left,
                    top_left: image.uv_rect.top_right,
                    bottom_left: image.uv_rect.bottom_right,
                    bottom_right: image.uv_rect.bottom_left,
                }
            }
            (BasicRotationAngle::Clockwise90, false) => {
                RectUv {
                    top_right: image.uv_rect.top_left,
                    bottom_right: image.uv_rect.top_right,
                    bottom_left: image.uv_rect.bottom_right,
                    top_left: image.uv_rect.bottom_left,
                }
            }
            (BasicRotationAngle::Clockwise180, _) => {
                RectUv {
                    bottom_right: image.uv_rect.top_left,
                    bottom_left: image.uv_rect.top_right,
                    top_left: image.uv_rect.bottom_right,
                    top_right: image.uv_rect.bottom_left,
                }
            }
            (BasicRotationAngle::Clockwise270, true) => {
                RectUv {
                    bottom_left: image.uv_rect.top_left,
                    bottom_right: image.uv_rect.top_right,
                    top_right: image.uv_rect.bottom_right,
                    top_left: image.uv_rect.bottom_left,
                }
            }
            (BasicRotationAngle::Clockwise270, false) => {
                RectUv {
                    bottom_left: image.uv_rect.top_left,
                    top_left: image.uv_rect.top_right,
                    top_right: image.uv_rect.bottom_right,
                    bottom_right: image.uv_rect.bottom_left,
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct PolygonPosColorUv {
    pub vertices: Vec<WorkVertex>,
}

#[derive(PartialEq, Eq, Hash)]
pub struct Glyph {
    pub size: Au,
    pub blur_radius: Au,
    pub index: u32,
}

impl Glyph {
    #[inline]
    pub fn new(size: Au, blur_radius: Au, index: u32) -> Glyph {
        Glyph {
            size: size,
            blur_radius: blur_radius,
            index: index,
        }
    }
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct PackedVertexForTextureCacheUpdate {
    pub x: f32,
    pub y: f32,
    pub color: PackedColor,
    pub u: u16,
    pub v: u16,
    pub border_radii_outer_rx: f32,
    pub border_radii_outer_ry: f32,
    pub border_radii_inner_rx: f32,
    pub border_radii_inner_ry: f32,
    pub border_position_x: f32,
    pub border_position_y: f32,
    pub border_position_arc_center_x: f32,
    pub border_position_arc_center_y: f32,
    pub dest_texture_size_x: f32,
    pub dest_texture_size_y: f32,
    pub source_texture_size_x: f32,
    pub source_texture_size_y: f32,
    pub blur_radius: f32,
    pub misc0: u8,
    pub uv_index: u8,
    pub misc2: u8,
    pub misc3: u8,
}

impl PackedVertexForTextureCacheUpdate {
    pub fn new(position: &Point2D<f32>,
               color: &ColorF,
               uv: &Point2D<f32>,
               uv_index: TextureIndex,
               border_radii_outer: &Point2D<f32>,
               border_radii_inner: &Point2D<f32>,
               border_position: &Point2D<f32>,
               border_position_arc_center: &Point2D<f32>,
               dest_texture_size: &Size2D<f32>,
               source_texture_size: &Size2D<f32>,
               blur_radius: f32)
               -> PackedVertexForTextureCacheUpdate {
        PackedVertexForTextureCacheUpdate {
            x: position.x,
            y: position.y,
            color: PackedColor::from_color(color),
            u: (uv.x * UV_FLOAT_TO_FIXED).round() as u16,
            v: (uv.y * UV_FLOAT_TO_FIXED).round() as u16,
            border_radii_outer_rx: border_radii_outer.x,
            border_radii_outer_ry: border_radii_outer.y,
            border_radii_inner_rx: border_radii_inner.x,
            border_radii_inner_ry: border_radii_inner.y,
            border_position_x: border_position.x,
            border_position_y: border_position.y,
            border_position_arc_center_x: border_position_arc_center.x,
            border_position_arc_center_y: border_position_arc_center.y,
            dest_texture_size_x: dest_texture_size.width,
            dest_texture_size_y: dest_texture_size.height,
            source_texture_size_x: source_texture_size.width,
            source_texture_size_y: source_texture_size.height,
            blur_radius: blur_radius,
            misc0: 0,
            uv_index: uv_index.0 as u8,
            misc2: 0,
            misc3: 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum TextureTarget {
    Texture2D,
    TextureArray,
}

impl TextureTarget {
    pub fn to_gl(&self) -> gl::GLuint {
        match *self {
            TextureTarget::Texture2D => gl::TEXTURE_2D,
            TextureTarget::TextureArray => gl::TEXTURE_2D_ARRAY,
        }
    }
}

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct BorderRadiusRasterOp {
    pub outer_radius_x: Au,
    pub outer_radius_y: Au,
    pub inner_radius_x: Au,
    pub inner_radius_y: Au,
    pub index: u32,
    pub image_format: ImageFormat,
    pub inverted: bool,
}

impl BorderRadiusRasterOp {
    pub fn create(outer_radius: &Size2D<f32>,
                  inner_radius: &Size2D<f32>,
                  inverted: bool,
                  index: u32,
                  image_format: ImageFormat)
                  -> Option<BorderRadiusRasterOp> {
        if outer_radius.width > 0.0 || outer_radius.height > 0.0 {
            Some(BorderRadiusRasterOp {
                outer_radius_x: Au::from_f32_px(outer_radius.width),
                outer_radius_y: Au::from_f32_px(outer_radius.height),
                inner_radius_x: Au::from_f32_px(inner_radius.width),
                inner_radius_y: Au::from_f32_px(inner_radius.height),
                index: index,
                inverted: inverted,
                image_format: image_format,
            })
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct BoxShadowRasterOp {
    pub blur_radius: Au,
    pub part: BoxShadowPart,
    pub raster_size: Au,
    pub inverted: bool,
}

impl BoxShadowRasterOp {
    pub fn raster_size(blur_radius: f32, border_radius: f32) -> f32 {
        (3.0 * blur_radius).max(border_radius) + 3.0 * blur_radius
    }

    pub fn create_corner(blur_radius: f32, border_radius: f32, inverted: bool)
                         -> Option<BoxShadowRasterOp> {
        if blur_radius > 0.0 || border_radius > 0.0 {
            Some(BoxShadowRasterOp {
                blur_radius: Au::from_f32_px(blur_radius),
                part: BoxShadowPart::Corner(Au::from_f32_px(border_radius)),
                raster_size: Au::from_f32_px(BoxShadowRasterOp::raster_size(blur_radius,
                                                                            border_radius)),
                inverted: inverted,
            })
        } else {
            None
        }
    }

    pub fn create_edge(blur_radius: f32, border_radius: f32, inverted: bool)
                       -> Option<BoxShadowRasterOp> {
        if blur_radius > 0.0 {
            Some(BoxShadowRasterOp {
                blur_radius: Au::from_f32_px(blur_radius),
                part: BoxShadowPart::Edge,
                raster_size: Au::from_f32_px(BoxShadowRasterOp::raster_size(blur_radius,
                                                                            border_radius)),
                inverted: inverted,
            })
        } else {
            None
        }
    }
}

#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub struct GlyphKey {
    pub font_key: FontKey,
    pub size: Au,
    pub blur_radius: Au,
    pub index: u32,
}

impl GlyphKey {
    pub fn new(font_key: FontKey, size: Au, blur_radius: Au, index: u32) -> GlyphKey {
        GlyphKey {
            font_key: font_key,
            size: size,
            blur_radius: blur_radius,
            index: index,
        }
    }
}

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub enum RasterItem {
    BorderRadius(BorderRadiusRasterOp),
    BoxShadow(BoxShadowRasterOp),
}

#[derive(Clone, Copy, Debug)]
pub enum BasicRotationAngle {
    Upright,
    Clockwise90,
    Clockwise180,
    Clockwise270,
}

#[derive(Debug, Clone, Copy)]
pub struct CombinedClipRegion<'a> {
    pub clip_in_rect: Rect<f32>,
    pub clip_in_complex: Option<ComplexClipRegion>,
    pub clip_in_complex_stack: &'a [ComplexClipRegion],
    pub clip_out_complex: Option<ComplexClipRegion>,
}

impl<'a> CombinedClipRegion<'a> {
    pub fn from_clip_in_rect_and_stack<'b>(clip_in_rect: &Rect<f32>,
                                           clip_in_complex_stack: &'b [ComplexClipRegion])
                                           -> CombinedClipRegion<'b> {
        CombinedClipRegion {
            clip_in_rect: *clip_in_rect,
            clip_in_complex: None,
            clip_in_complex_stack: clip_in_complex_stack,
            clip_out_complex: None,
        }
    }

    pub fn clip_in_rect(&mut self, rect: &Rect<f32>) -> &mut CombinedClipRegion<'a> {
        self.clip_in_rect = self.clip_in_rect.intersection(rect).unwrap_or(ZERO_RECT_F32);
        self
    }

    pub fn clip_in(&mut self, region: &ComplexClipRegion) -> &mut CombinedClipRegion<'a> {
        debug_assert!(self.clip_in_complex.is_none());
        self.clip_in_complex = Some(*region);
        self
    }

    pub fn clip_out(&mut self, region: &ComplexClipRegion) -> &mut CombinedClipRegion<'a> {
        debug_assert!(self.clip_out_complex.is_none());
        self.clip_out_complex = Some(*region);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LowLevelFilterOp {
    Blur(Au, BlurDirection),
    Brightness(Au),
    Contrast(Au),
    Grayscale(Au),
    /// Fixed-point in `ANGLE_FLOAT_TO_FIXED` units.
    HueRotate(i32),
    Invert(Au),
    Opacity(Au),
    Saturate(Au),
    Sepia(Au),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CompositionOp {
    MixBlend(MixBlendMode),
    Filter(LowLevelFilterOp),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BlurDirection {
    Horizontal,
    Vertical,
}
