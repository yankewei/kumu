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
const MAX_CACHED_DIAGRAMS: usize = 64;

struct MermaidBlock {
    source: SharedString,
    offset: usize,
}

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
        parse_diagram(node, cx.source(), cx.offset())
    }

    fn render(&self, node: &MarkdownNode, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Some(block) = node.data::<MermaidBlock>() else {
            return div().into_any_element();
        };
        let revision = cx.default_global::<crate::theme::ThemeRevision>().0;
        if cx
            .try_global::<DiagramTheme>()
            .is_none_or(|theme| theme.revision != revision)
        {
            let spec = Arc::new(ThemeSpec::from_theme(cx.theme()));
            cx.set_global(DiagramTheme { revision, spec });
        }
        let key = DiagramKey {
            source: block.source.clone(),
            theme: revision,
        };

        match diagram(key, window, cx) {
            DiagramState::Ready(diagram) => {
                let theme = cx.theme();
                let expand = Button::new(("mermaid-expand", block.offset))
                    .ghost()
                    .small()
                    .icon(IconName::Maximize)
                    .tooltip("放大查看")
                    .on_click({
                        let diagram = diagram.clone();
                        move |_, window, cx| {
                            cx.stop_propagation();
                            open_viewer(diagram.clone(), window, cx);
                        }
                    });
                div()
                    .id((NODE_NAME, block.offset))
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
            // Keep the source readable while the diagram is rendering.
            DiagramState::Pending | DiagramState::Failed => {
                let id = ElementId::from((NODE_NAME, block.offset));
                TextView::markdown(id, node.as_markdown().to_string()).into_any_element()
            }
        }
    }
}

fn parse_diagram(node: &markdown_ast::Node, source: &str, offset: usize) -> Option<MarkdownNode> {
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
    let position = node.position()?;
    let fence = source.get(position.start.offset..position.end.offset)?;
    let prefix = source[..position.start.offset].rsplit('\n').next()?;
    let quote_depth = prefix.chars().filter(|c| *c == '>').count();
    if !is_closed_fence(fence, quote_depth) {
        return None;
    }
    Some(
        MarkdownNode::new(
            NODE_NAME,
            MermaidBlock {
                source: code.value.clone().into(),
                offset: offset + position.start.offset,
            },
        )
        .text(code.value.clone())
        .markdown(fence),
    )
}

/// A fence's source slice ends with its closing marker; one cut off mid-stream
/// ends with the last line of the diagram instead.
fn is_closed_fence(fence: &str, quote_depth: usize) -> bool {
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
    // mdast excludes the opening container prefix, but keeps it on later lines.
    let mut closing = rest.lines().last().unwrap_or_default().trim();
    for _ in 0..quote_depth {
        let Some(line) = closing.strip_prefix('>') else {
            return false;
        };
        closing = line.trim_start();
    }
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
    theme: u64,
}

struct DiagramTheme {
    revision: u64,
    spec: Arc<ThemeSpec>,
}

impl Global for DiagramTheme {}

struct CachedDiagram {
    state: DiagramState,
    last_used: u64,
    request: u64,
}

#[derive(Default)]
struct Diagrams {
    entries: HashMap<DiagramKey, CachedDiagram>,
    clock: u64,
}

impl Global for Diagrams {}

impl Diagrams {
    fn evict(&mut self, theme: u64) -> Vec<Arc<RenderImage>> {
        let mut stale = Vec::new();
        self.entries.retain(|key, entry| {
            if key.theme == theme {
                return true;
            }
            if let DiagramState::Ready(diagram) = &entry.state {
                stale.push(diagram.image.clone());
            }
            false
        });
        while self.entries.len() >= MAX_CACHED_DIAGRAMS {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
                .unwrap();
            if let Some(CachedDiagram {
                state: DiagramState::Ready(diagram),
                ..
            }) = self.entries.remove(&oldest)
            {
                stale.push(diagram.image);
            }
        }
        stale
    }
}

fn diagram(key: DiagramKey, window: &mut Window, cx: &mut App) -> DiagramState {
    let diagrams = cx.default_global::<Diagrams>();
    diagrams.clock += 1;
    let request = diagrams.clock;
    if let Some(entry) = diagrams.entries.get_mut(&key) {
        entry.last_used = request;
        return entry.state.clone();
    }
    let stale = diagrams.evict(key.theme);
    diagrams.entries.insert(
        key.clone(),
        CachedDiagram {
            state: DiagramState::Pending,
            last_used: request,
            request,
        },
    );
    for image in stale {
        cx.drop_image(image, Some(window));
    }

    let svg_renderer = cx.svg_renderer();
    let theme = cx.global::<DiagramTheme>().spec.clone();
    let job = key.clone();
    let task = cx.background_spawn(async move {
        let svg = render_svg(&job.source, &theme)?;
        let image = svg_renderer
            .render_single_frame(svg.as_bytes(), 1.)
            .map_err(|error| eprintln!("Mermaid rasterisation failed: {error}"))
            .ok()?;
        Some(Diagram {
            svg: svg.into(),
            image,
        })
    });
    cx.spawn(async move |cx| {
        let diagram = task.await;
        cx.update(|cx| {
            let state = diagram.map_or(DiagramState::Failed, DiagramState::Ready);
            // An evicted source may have been requested again while this job ran.
            if let Some(entry) = cx.default_global::<Diagrams>().entries.get_mut(&key)
                && entry.request == request
            {
                entry.state = state;
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
        .with_theme(theme.host_theme().or_else(|| {
            eprintln!("Mermaid theme validation failed");
            None
        })?)
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
        Err(error) => {
            eprintln!("Mermaid SVG rendering failed: {error}");
            None
        }
        _ => {
            eprintln!("Mermaid renderer returned no SVG");
            None
        }
    }
}

/// The app theme as merman's semantic roles, rebuilt only when the theme changes.
struct ThemeSpec {
    dark: bool,
    roles: Vec<(ThemeRole, String)>,
    series: Vec<String>,
}

impl ThemeSpec {
    fn from_theme(theme: &gpui_kit::component::Theme) -> Self {
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
    use gpui_kit::component::{Theme, ThemeMode};

    use super::{
        CachedDiagram, DiagramKey, DiagramState, Diagrams, MAX_CACHED_DIAGRAMS, MermaidBlock,
        ThemeSpec, is_closed_fence, parse_diagram, render_svg,
    };

    #[test]
    fn a_fence_counts_as_closed_only_with_its_closing_marker() {
        assert!(is_closed_fence("```mermaid\ngraph TD\nA-->B\n```", 0));
        assert!(is_closed_fence("````mermaid\ngraph TD\n````\n", 0));
        assert!(!is_closed_fence("```mermaid\ngraph TD\nA-->B", 0));
        assert!(!is_closed_fence("````mermaid\ngraph TD\n```", 0));
    }

    #[test]
    fn parsed_fences_are_claimed_only_after_closing() {
        for (source, claimed) in [
            ("```mermaid\ngraph TD\nA-->B", false),
            ("```mermaid\ngraph TD\nA-->B\n```", true),
            ("> ```mermaid\n> graph TD\n> ```", true),
            ("> ```mermaid\n> graph TD", false),
            ("- ```mermaid\n  graph TD\n  ```", true),
            ("> - > ```mermaid\n>   > graph TD\n>   > ```", true),
            ("```mermaid\ngraph TD\n> ```", false),
        ] {
            let tree = markdown::to_mdast(source, &markdown::ParseOptions::gfm()).unwrap();
            let mut node = &tree;
            while let Some(children) = node.children() {
                node = &children[0];
            }
            assert!(matches!(node, markdown::mdast::Node::Code(_)));
            let parsed = parse_diagram(node, source, 100);
            assert_eq!(parsed.is_some(), claimed, "{source}");
            if let Some(parsed) = parsed {
                let block = parsed.data::<MermaidBlock>().unwrap();
                assert_eq!(block.offset, 100 + node.position().unwrap().start.offset);
                assert!(!block.source.is_empty());
            }
        }
    }

    #[test]
    fn cache_evicts_the_least_recently_used_entry_and_old_themes() {
        let mut cache = Diagrams::default();
        for index in 0..MAX_CACHED_DIAGRAMS {
            cache.entries.insert(
                DiagramKey {
                    source: index.to_string().into(),
                    theme: 1,
                },
                CachedDiagram {
                    state: DiagramState::Pending,
                    last_used: index as u64,
                    request: index as u64,
                },
            );
        }
        let first = DiagramKey {
            source: "0".into(),
            theme: 1,
        };
        cache.entries.get_mut(&first).unwrap().last_used = MAX_CACHED_DIAGRAMS as u64;
        cache.evict(1);
        assert_eq!(cache.entries.len(), MAX_CACHED_DIAGRAMS - 1);
        assert!(cache.entries.contains_key(&first));
        assert!(!cache.entries.contains_key(&DiagramKey {
            source: "1".into(),
            theme: 1
        }));
        cache.evict(2);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn all_app_theme_roles_are_accepted() {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let theme = Theme {
                mode,
                ..Theme::default()
            };
            let spec = ThemeSpec::from_theme(&theme);
            assert_eq!(spec.roles.len(), 22);
            assert_eq!(spec.series.len(), 5);
            assert!(spec.host_theme().is_some());
        }
    }

    /// A flowchart has to survive the whole path to pixels: merman must not
    /// ask for a layout engine that is not compiled in, and the image must
    /// leave the reply's background showing rather than paint its own.
    #[test]
    fn a_flowchart_rasterises_on_a_transparent_background() {
        let theme = ThemeSpec::from_theme(&Theme::default());
        let svg = render_svg("flowchart LR\n  A[开始] --> B{判断}", &theme).expect("svg");
        let image = SvgRenderer::new(Arc::new(()))
            .render_single_frame(svg.as_bytes(), 1.)
            .expect("image");
        let corner = &image.as_bytes(0).expect("frame")[..4];
        assert_eq!(corner[3], 0, "the corner should be transparent");
    }
}
