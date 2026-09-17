//! A dialog for reading a Mermaid diagram too big for the reply column.
//!
//! The diagram is an image placed inside a clipped viewport at `offset`, drawn
//! at `zoom` times its natural size. Dragging moves it, a pinch or the
//! platform modifier plus the wheel zooms around the pointer, and a plain wheel
//! pans. Zooming past the pixels the image was rasterised with renders the SVG
//! again at a higher scale in the background, so text stays sharp up close.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::Size;
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::*;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::markdown_mermaid::Diagram;

const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 8.;
const ZOOM_STEP: f32 = 1.25;
/// Dialog padding on each side, which the viewport sits inside.
const DIALOG_PADDING: f32 = 16.;

pub(super) fn open_viewer(diagram: Diagram, window: &mut Window, cx: &mut App) {
    let window_size = window.viewport_size();
    let width = (f32::from(window_size.width) - 96.).clamp(320., 1280.);
    let height = (f32::from(window_size.height) * 0.7).max(240.);
    let viewer = cx.new(|cx| DiagramViewer::new(diagram, size(width, height), cx));
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("图表")
            .w(px(width + DIALOG_PADDING * 2.))
            .child(viewer.clone())
    });
}

struct DiagramViewer {
    svg: Arc<str>,
    /// The image from the reply, which the cache owns.
    base: Arc<RenderImage>,
    /// A sharper copy rendered for this dialog, and the scale it was rendered
    /// at relative to the natural size. The dialog owns it and frees it.
    sharp: Option<(Arc<RenderImage>, f32)>,
    /// The scale of a render still in flight, so zooming does not queue more.
    rendering: Option<f32>,
    natural: Size<f32>,
    viewport: Size<f32>,
    /// Where the viewport sits in the window, read back after layout; pointer
    /// positions arrive in window coordinates.
    origin: Rc<Cell<Point<Pixels>>>,
    zoom: f32,
    offset: Point<f32>,
    drag: Option<Point<Pixels>>,
}

impl DiagramViewer {
    fn new(diagram: Diagram, viewport: Size<f32>, cx: &mut Context<Self>) -> Self {
        let pixels = diagram.image.size(0);
        let natural = size(
            pixels.width.0 as f32 / SMOOTH_SVG_SCALE_FACTOR,
            pixels.height.0 as f32 / SMOOTH_SVG_SCALE_FACTOR,
        );
        cx.on_release(|this, cx| {
            if let Some((image, _)) = this.sharp.take() {
                cx.drop_image(image, None);
            }
        })
        .detach();

        let mut viewer = Self {
            svg: diagram.svg,
            base: diagram.image,
            sharp: None,
            rendering: None,
            natural,
            viewport,
            origin: Rc::default(),
            zoom: 1.,
            offset: point(0., 0.),
            drag: None,
        };
        viewer.fit();
        viewer
    }

    /// Show the whole diagram, centred, without enlarging a small one.
    fn fit(&mut self) {
        let zoom = (self.viewport.width / self.natural.width)
            .min(self.viewport.height / self.natural.height)
            .min(1.)
            .clamp(MIN_ZOOM, MAX_ZOOM);
        self.zoom = zoom;
        self.offset = point(
            (self.viewport.width - self.natural.width * zoom) / 2.,
            (self.viewport.height - self.natural.height * zoom) / 2.,
        );
    }

    /// Scale by `factor` keeping the diagram point under `anchor` (viewport
    /// coordinates) where it is.
    fn zoom_by(
        &mut self,
        factor: f32,
        anchor: Point<f32>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let applied = zoom / self.zoom;
        self.offset = point(
            anchor.x - (anchor.x - self.offset.x) * applied,
            anchor.y - (anchor.y - self.offset.y) * applied,
        );
        self.zoom = zoom;
        self.sharpen(window, cx);
        cx.notify();
    }

    fn zoom_at_center(&mut self, factor: f32, window: &Window, cx: &mut Context<Self>) {
        let center = point(self.viewport.width / 2., self.viewport.height / 2.);
        self.zoom_by(factor, center, window, cx);
    }

    fn local(&self, position: Point<Pixels>) -> Point<f32> {
        let origin = self.origin.get();
        point(
            f32::from(position.x - origin.x),
            f32::from(position.y - origin.y),
        )
    }

    /// The image carries twice its natural pixels at scale 1; once the zoomed
    /// size needs more device pixels than that, render at the next power of
    /// two so a zoom gesture triggers a few renders rather than one per step.
    fn sharpen(&mut self, window: &Window, cx: &mut Context<Self>) {
        let needed = self.zoom * window.scale_factor() / SMOOTH_SVG_SCALE_FACTOR;
        let have = self.sharp.as_ref().map_or(1., |(_, scale)| *scale);
        if needed <= have || self.rendering.is_some_and(|scale| scale >= needed) {
            return;
        }
        let scale = needed.ceil().max(1.) as u32;
        let scale = scale.next_power_of_two() as f32;
        self.rendering = Some(scale);

        let svg = self.svg.clone();
        let renderer = cx.svg_renderer();
        let task =
            cx.background_spawn(
                async move { renderer.render_single_frame(svg.as_bytes(), scale).ok() },
            );
        cx.spawn(async move |this, cx| {
            let image = task.await;
            let unused = this
                .update(cx, |this, cx| {
                    this.rendering = None;
                    let image = image?;
                    let old = this.sharp.replace((image, scale));
                    cx.notify();
                    old.map(|(image, _)| image)
                })
                .unwrap_or(None);
            if let Some(image) = unused {
                cx.update(|cx| cx.drop_image(image, None));
            }
        })
        .detach();
    }
}

impl Render for DiagramViewer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let image = self
            .sharp
            .as_ref()
            .map_or(self.base.clone(), |(image, _)| image.clone());
        let origin = self.origin.clone();

        let toolbar = h_flex()
            .absolute()
            .bottom_3()
            .right_3()
            .items_center()
            .gap_1()
            .p_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .shadow_sm()
            .child(
                div()
                    .px_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if cfg!(target_os = "macos") {
                        "拖动平移 · ⌘ / Ctrl + 滚轮缩放"
                    } else {
                        "拖动平移 · Ctrl + 滚轮缩放"
                    }),
            )
            .child(
                Button::new("mermaid-zoom-out")
                    .ghost()
                    .small()
                    .icon(IconName::Minus)
                    .tooltip("缩小")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.zoom_at_center(1. / ZOOM_STEP, window, cx)
                    })),
            )
            .child(
                div()
                    .min_w_10()
                    .text_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("{:.0}%", self.zoom * 100.)),
            )
            .child(
                Button::new("mermaid-zoom-in")
                    .ghost()
                    .small()
                    .icon(IconName::Plus)
                    .tooltip("放大")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.zoom_at_center(ZOOM_STEP, window, cx)
                    })),
            )
            .child(
                Button::new("mermaid-fit")
                    .ghost()
                    .small()
                    .icon(IconName::RotateCw)
                    .tooltip("适应窗口")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.fit();
                        cx.notify();
                    })),
            );

        div()
            .id("mermaid-viewport")
            .relative()
            .w(px(self.viewport.width))
            .h(px(self.viewport.height))
            .overflow_hidden()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .cursor(if self.drag.is_some() {
                CursorStyle::ClosedHand
            } else {
                CursorStyle::OpenHand
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, _| {
                    this.drag = Some(event.position);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                let Some(last) = this.drag else {
                    return;
                };
                this.offset.x += f32::from(event.position.x - last.x);
                this.offset.y += f32::from(event.position.y - last.y);
                this.drag = Some(event.position);
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.drag = None;
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.drag = None;
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                let delta = event.delta.pixel_delta(window.line_height());
                if event.modifiers.secondary() || event.modifiers.control {
                    let factor = (f32::from(delta.y) / 200.).exp();
                    let anchor = this.local(event.position);
                    this.zoom_by(factor, anchor, window, cx);
                } else {
                    this.offset.x += f32::from(delta.x);
                    this.offset.y += f32::from(delta.y);
                    cx.notify();
                }
                cx.stop_propagation();
            }))
            .on_pinch(cx.listener(|this, event: &PinchEvent, window, cx| {
                let anchor = this.local(event.position);
                this.zoom_by(1. + event.delta, anchor, window, cx);
            }))
            .child(
                canvas(
                    move |bounds, _, _| origin.set(bounds.origin),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                img(image)
                    .absolute()
                    .left(px(self.offset.x))
                    .top(px(self.offset.y))
                    .w(px(self.natural.width * self.zoom))
                    .h(px(self.natural.height * self.zoom)),
            )
            .child(toolbar)
    }
}
