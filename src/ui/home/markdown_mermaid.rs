//! Mermaid blocks in Markdown replies, drawn as images.
//!
//! merman lays the diagram out and writes a raster-safe SVG on a background
//! thread; GPUI's own resvg turns that into an image. A fence is claimed only
//! once it is closed, so a diagram still being streamed stays an ordinary code
//! block instead of being re-rendered on every chunk. A diagram merman cannot
//! render also falls back to that code block.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::text::{
    MarkdownNode, MarkdownParseContext, MarkdownPlugin, TextView, markdown_ast,
};
use gpui_kit::component::{IconName, Sizable};
use gpui_kit::prelude::*;
use gpui_kit::*;
use merman::svg::{
    HostTheme, HostThemeAppearance, Presentation, RootBackgroundPostprocessor, SvgPipeline,
    ThemeRole,
};
use merman::{OperationControl, RenderOutput, RenderRequest, Renderer, SvgRequest};

use super::mermaid_viewer::open_viewer;

const NODE_NAME: &str = "kumu-mermaid";

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MermaidMarkdownPlugin;

impl MarkdownPlugin for MermaidMarkdownPlugin {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        NODE_NAME
    }

    fn parse(
        &self,
        node: &markdown_ast::Node,
        cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let markdown_ast::Node::Code(code) = node else {
            return None;
        };
        if !code
            .lang
            .as_deref()
            .is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid"))
        {
            return None;
        }
        let fence = cx.node_source(node)?;
        if !is_closed_fence(fence) {
            return None;
        }
        Some(
            MarkdownNode::new(NODE_NAME, SharedString::from(code.value.clone()))
                .text(code.value.clone())
                .markdown(fence),
        )
    }

    fn render(&self, node: &MarkdownNode, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Some(source) = node.data::<SharedString>() else {
            return div().into_any_element();
        };
        let key = DiagramKey {
            source: source.clone(),
            theme: ThemeSpec::from_app(cx),
        };

        match diagram(key, cx) {
            DiagramState::Ready(diagram) => {
                let theme = cx.theme();
                let id = format!("{NODE_NAME}-{:p}", source.as_ptr());
                let expand = Button::new(SharedString::from(format!("{id}-expand")))
                    .ghost()
                    .small()
                    .icon(IconName::Maximize)
                    .tooltip("放大查看")
                    .on_click({
                        let diagram = diagram.clone();
                        move |_, window, cx| open_viewer(diagram.clone(), window, cx)
                    });
                div()
                    .id(SharedString::from(id))
                    .relative()
                    .w_full()
                    .p_3()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .on_click({
                        let diagram = diagram.clone();
                        move |_, window, cx| open_viewer(diagram.clone(), window, cx)
                    })
                    .child(img(diagram.image.clone()).max_w_full())
                    .child(div().absolute().top_1().right_1().child(expand))
                    .into_any_element()
            }
            // Laying out takes a moment; the fence reads as code meanwhile
            // rather than as an empty box that jumps once the image lands.
            DiagramState::Pending | DiagramState::Failed => {
                let id = ElementId::Name(format!("{NODE_NAME}-{:p}", source.as_ptr()).into());
                TextView::markdown(id, node.as_markdown().to_string()).into_any_element()
            }
        }
    }
}

/// A fence's source slice ends with its closing marker; one cut off mid-stream
/// ends with the last line of the diagram instead.
fn is_closed_fence(fence: &str) -> bool {
    let fence = fence.trim_end();
    let Some((opening, rest)) = fence.split_once('\n') else {
        return false;
    };
    let opening = opening.trim_start();
    let marker = opening.chars().next().filter(|c| matches!(c, '`' | '~'));
    let Some(marker) = marker else {
        return false;
    };
    let width = opening.chars().take_while(|c| *c == marker).count();
    let closing = rest.lines().last().unwrap_or_default().trim();
    closing.len() >= width && closing.chars().all(|c| c == marker)
}

#[derive(Clone)]
enum DiagramState {
    Pending,
    Ready(Diagram),
    Failed,
}

/// A rendered diagram: the image drawn in the reply, and the SVG it came from
/// so the viewer can rasterise it again at a higher resolution.
#[derive(Clone)]
pub(super) struct Diagram {
    pub(super) svg: Arc<str>,
    /// Rasterised at the diagram's natural size (GPUI doubles the pixels).
    pub(super) image: Arc<RenderImage>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct DiagramKey {
    source: SharedString,
    theme: ThemeSpec,
}

/// Every diagram rendered so far, for the current theme. Rendering happens once
/// per source; switching theme drops the old images and renders again.
#[derive(Default)]
struct Diagrams(HashMap<DiagramKey, DiagramState>);

impl Global for Diagrams {}

fn diagram(key: DiagramKey, cx: &mut App) -> DiagramState {
    if let Some(state) = cx
        .try_global::<Diagrams>()
        .and_then(|diagrams| diagrams.0.get(&key))
    {
        return state.clone();
    }

    let mut stale = Vec::new();
    let diagrams = cx.default_global::<Diagrams>();
    diagrams.0.retain(|cached, state| {
        let keep = cached.theme == key.theme;
        if !keep && let DiagramState::Ready(diagram) = state {
            stale.push(diagram.image.clone());
        }
        keep
    });
    diagrams.0.insert(key.clone(), DiagramState::Pending);
    for image in stale {
        cx.drop_image(image, None);
    }

    let svg_renderer = cx.svg_renderer();
    let job = key.clone();
    let task = cx.background_spawn(async move {
        let svg = render_svg(&job.source, &job.theme)?;
        let image = svg_renderer.render_single_frame(svg.as_bytes(), 1.).ok()?;
        Some(Diagram {
            svg: svg.into(),
            image,
        })
    });
    cx.spawn(async move |cx| {
        let diagram = task.await;
        cx.update(|cx| {
            let state = diagram.map_or(DiagramState::Failed, DiagramState::Ready);
            // A theme switch while this was rendering already evicted the key.
            if let Some(slot) = cx.default_global::<Diagrams>().0.get_mut(&key) {
                *slot = state;
                cx.refresh_windows();
            } else if let DiagramState::Ready(diagram) = state {
                cx.drop_image(diagram.image, None);
            }
        });
    })
    .detach();

    DiagramState::Pending
}

fn render_svg(source: &str, theme: &ThemeSpec) -> Option<String> {
    let presentation = Presentation::new()
        .with_theme(theme.host_theme()?)
        .resolve();
    let renderer =
        Renderer::new().with_engine(presentation.materialize_engine(merman::Engine::new()));
    let request = SvgRequest {
        // merman paints the root white through `background-color`, which usvg
        // honours; the reply's own background shows through instead.
        pipeline: Some(
            SvgPipeline::resvg_safe()
                .with_postprocessor(RootBackgroundPostprocessor::new("transparent")),
        ),
        presentation: presentation.render_policy(),
        ..Default::default()
    };
    match renderer.render(RenderRequest::svg(source, OperationControl::new(), request)) {
        Ok(RenderOutput::Svg(Some(svg))) => Some(svg.into_parts().0),
        _ => None,
    }
}

/// The app theme as merman's semantic roles, in CSS colours. It is also the
/// cache key, so a diagram is rendered again only when one of these changes.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ThemeSpec {
    dark: bool,
    roles: Vec<(ThemeRole, String)>,
    series: Vec<String>,
}

impl ThemeSpec {
    fn from_app(cx: &App) -> Self {
        let theme = cx.theme();
        let roles = [
            (ThemeRole::Canvas, theme.background),
            (ThemeRole::Surface, theme.muted),
            (ThemeRole::SurfaceAlt, theme.secondary),
            (ThemeRole::SurfaceMuted, theme.accent),
            (ThemeRole::Text, theme.foreground),
            (ThemeRole::SubtleText, theme.muted_foreground),
            (ThemeRole::Border, theme.border),
            (ThemeRole::Line, theme.muted_foreground),
            (ThemeRole::EdgeLabelBackground, theme.background),
            (ThemeRole::ClusterBackground, theme.muted.opacity(0.5)),
            (ThemeRole::ClusterBorder, theme.border),
            (ThemeRole::NoteBackground, theme.secondary),
            (ThemeRole::NoteBorder, theme.border),
            (ThemeRole::NoteText, theme.foreground),
            (ThemeRole::ActorBackground, theme.muted),
            (ThemeRole::ActorBorder, theme.border),
            (ThemeRole::ActorText, theme.foreground),
            (ThemeRole::ActivationBackground, theme.secondary),
            (ThemeRole::ActivationBorder, theme.border),
            (ThemeRole::Error, theme.danger),
            (ThemeRole::Warning, theme.warning),
            (ThemeRole::Success, theme.success),
        ];
        let series = [
            theme.chart_1,
            theme.chart_2,
            theme.chart_3,
            theme.chart_4,
            theme.chart_5,
        ];
        Self {
            dark: theme.mode.is_dark(),
            roles: roles
                .into_iter()
                .map(|(role, color)| (role, css_color(color)))
                .collect(),
            series: series.into_iter().map(css_color).collect(),
        }
    }

    fn host_theme(&self) -> Option<HostTheme> {
        let appearance = if self.dark {
            HostThemeAppearance::Dark
        } else {
            HostThemeAppearance::Light
        };
        // GPUI's SVG renderer resolves a family fontdb does not know to the
        // system sans-serif, so `system-ui` lands on the UI font rather than
        // Mermaid's default Trebuchet, which macOS does ship.
        let mut theme = HostTheme::new()
            .with_appearance(appearance)
            .try_with_font_family("system-ui, sans-serif")
            .ok()?;
        for (role, color) in &self.roles {
            theme = theme.try_with_role(*role, color.as_str()).ok()?;
        }
        theme.try_with_series_palette(&self.series).ok()
    }
}

fn css_color(color: Hsla) -> String {
    let Rgba { r, g, b, a } = color.to_rgb();
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    format!(
        "rgba({}, {}, {}, {a:.3})",
        channel(r),
        channel(g),
        channel(b)
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui_kit::SvgRenderer;
    use merman::svg::ThemeRole;

    use super::{ThemeSpec, is_closed_fence, render_svg};

    #[test]
    fn a_fence_counts_as_closed_only_with_its_closing_marker() {
        assert!(is_closed_fence("```mermaid\ngraph TD\nA-->B\n```"));
        assert!(is_closed_fence("````mermaid\ngraph TD\n````\n"));
        assert!(!is_closed_fence("```mermaid\ngraph TD\nA-->B"));
        assert!(!is_closed_fence("````mermaid\ngraph TD\n```"));
    }

    /// A flowchart has to survive the whole path to pixels: merman must not
    /// ask for a layout engine that is not compiled in, and the image must
    /// leave the reply's background showing rather than paint its own.
    #[test]
    fn a_flowchart_rasterises_on_a_transparent_background() {
        let theme = ThemeSpec {
            dark: true,
            roles: vec![(ThemeRole::Text, "rgba(250, 250, 250, 1.000)".into())],
            series: Vec::new(),
        };
        let svg = render_svg("flowchart LR\n  A[开始] --> B{判断}", &theme).expect("svg");
        let image = SvgRenderer::new(Arc::new(()))
            .render_single_frame(svg.as_bytes(), 1.)
            .expect("image");
        let corner = &image.as_bytes(0).expect("frame")[..4];
        assert_eq!(corner[3], 0, "the corner should be transparent");
    }
}
