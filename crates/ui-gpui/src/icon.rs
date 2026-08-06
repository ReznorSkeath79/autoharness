//! AutoHarness's shared icon vocabulary.
//!
//! Transplanted from diri's `diri-ui/icon.rs` (Apache-2.0; see NOTICE), with
//! its SF Symbol compatibility bridge dropped — AutoHarness never spoke that
//! catalog, so names here are semantic from the start.
//!
//! Before this, the shell drew its controls with literal Unicode characters:
//! `⌕` for search, `♧` for notifications, `⚙` for settings, and the Command
//! key symbol `⌘` as a decorative mark on the Worktrees button, right beside a
//! real `⌘K` chip. Those glyphs are not in the UI font at a consistent optical
//! weight, so they rendered as illegible specks of different sizes — and one of
//! them told the user a keyboard shortcut that does not exist. Every glyph here
//! is a 24×24 SVG with a rounded stroke, tinted by `text_color`, so a control's
//! icon scales and aligns with the rest of the type system.

use std::borrow::Cow;

use gpui::{App, AssetSource, IntoElement, RenderOnce, Rgba, Window, prelude::*, px, svg};

/// Optical sizes for the shared 24×24 line icons. Three sizes, like the type
/// scale: a fourth is how a dense tool starts looking accidental.
pub struct IconSize;

impl IconSize {
    /// Supporting marks: chevrons, inline row actions.
    pub const COMPACT: f32 = 13.0;
    /// Row, toolbar, and navigation icons.
    pub const REGULAR: f32 = 15.0;
    /// Empty states and other display-size illustrations.
    pub const DISPLAY: f32 = 26.0;
}

/// The semantic icon vocabulary. Views name what an icon *means*, never a
/// glyph, so the whole app can restyle as one system.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IconName {
    Activity,
    Archive,
    ArrowDown,
    ArrowLeft,
    Branch,
    /// Brand marks are filled 24×24 glyphs (from diri's `brand.rs`, Apache-2.0;
    /// see NOTICE), used only to identify the integration they belong to.
    BrandClaude,
    BrandCopilot,
    BrandCursor,
    BrandGemini,
    BrandKimi,
    BrandOpenAi,
    BrandOpenCode,
    BrandPi,
    Check,
    CheckCircle,
    Checklist,
    ChevronDown,
    ChevronRight,
    ChevronUp,
    ChevronUpDown,
    Clock,
    Close,
    CloseCircle,
    Code,
    Comment,
    Cube,
    Download,
    ExternalLink,
    Folder,
    Grid,
    LocalAgents,
    Merge,
    Monitor,
    Moon,
    More,
    Network,
    NewAgent,
    Plus,
    Pointer,
    Power,
    PullRequest,
    Refresh,
    ResizeHorizontal,
    Search,
    Server,
    Settings,
    Sidebar,
    SidebarRight,
    Sparkle,
    Stack,
    Terminal,
    Trash,
    Unarchive,
    Warning,
    Worktree,
}

impl IconName {
    pub const ALL: [Self; 55] = [
        Self::Activity,
        Self::Archive,
        Self::ArrowDown,
        Self::ArrowLeft,
        Self::Branch,
        Self::BrandClaude,
        Self::BrandCopilot,
        Self::BrandCursor,
        Self::BrandGemini,
        Self::BrandKimi,
        Self::BrandOpenAi,
        Self::BrandOpenCode,
        Self::BrandPi,
        Self::Check,
        Self::CheckCircle,
        Self::Checklist,
        Self::ChevronDown,
        Self::ChevronRight,
        Self::ChevronUp,
        Self::ChevronUpDown,
        Self::Clock,
        Self::Close,
        Self::CloseCircle,
        Self::Code,
        Self::Comment,
        Self::Cube,
        Self::Download,
        Self::ExternalLink,
        Self::Folder,
        Self::Grid,
        Self::LocalAgents,
        Self::Merge,
        Self::Monitor,
        Self::Moon,
        Self::More,
        Self::Network,
        Self::NewAgent,
        Self::Plus,
        Self::Pointer,
        Self::Power,
        Self::PullRequest,
        Self::Refresh,
        Self::ResizeHorizontal,
        Self::Search,
        Self::Server,
        Self::Settings,
        Self::Sidebar,
        Self::SidebarRight,
        Self::Sparkle,
        Self::Stack,
        Self::Terminal,
        Self::Trash,
        Self::Unarchive,
        Self::Warning,
        Self::Worktree,
    ];

    pub const fn asset_path(self) -> &'static str {
        match self {
            Self::Activity => "icons/activity.svg",
            Self::Archive => "icons/archive.svg",
            Self::ArrowDown => "icons/arrow-down.svg",
            Self::ArrowLeft => "icons/arrow-left.svg",
            Self::Branch => "icons/branch.svg",
            Self::BrandClaude => "icons/brand-claude.svg",
            Self::BrandCopilot => "icons/brand-copilot.svg",
            Self::BrandCursor => "icons/brand-cursor.svg",
            Self::BrandGemini => "icons/brand-gemini.svg",
            Self::BrandKimi => "icons/brand-kimi.svg",
            Self::BrandOpenAi => "icons/brand-openai.svg",
            Self::BrandOpenCode => "icons/brand-opencode.svg",
            Self::BrandPi => "icons/brand-pi.svg",
            Self::Check => "icons/check.svg",
            Self::CheckCircle => "icons/check-circle.svg",
            Self::Checklist => "icons/checklist.svg",
            Self::ChevronDown => "icons/chevron-down.svg",
            Self::ChevronRight => "icons/chevron-right.svg",
            Self::ChevronUp => "icons/chevron-up.svg",
            Self::ChevronUpDown => "icons/chevron-up-down.svg",
            Self::Clock => "icons/clock.svg",
            Self::Close => "icons/close.svg",
            Self::CloseCircle => "icons/close-circle.svg",
            Self::Code => "icons/code.svg",
            Self::Comment => "icons/comment.svg",
            Self::Cube => "icons/cube.svg",
            Self::Download => "icons/download.svg",
            Self::ExternalLink => "icons/external-link.svg",
            Self::Folder => "icons/folder.svg",
            Self::Grid => "icons/grid.svg",
            Self::LocalAgents => "icons/local-agents.svg",
            Self::Merge => "icons/merge.svg",
            Self::Monitor => "icons/monitor.svg",
            Self::Moon => "icons/moon.svg",
            Self::More => "icons/more.svg",
            Self::Network => "icons/network.svg",
            Self::NewAgent => "icons/new-agent.svg",
            Self::Plus => "icons/plus.svg",
            Self::Pointer => "icons/pointer.svg",
            Self::Power => "icons/power.svg",
            Self::PullRequest => "icons/pull-request.svg",
            Self::Refresh => "icons/refresh.svg",
            Self::ResizeHorizontal => "icons/resize-horizontal.svg",
            Self::Search => "icons/search.svg",
            Self::Server => "icons/server.svg",
            Self::Settings => "icons/settings.svg",
            Self::Sidebar => "icons/sidebar.svg",
            Self::SidebarRight => "icons/sidebar-right.svg",
            Self::Sparkle => "icons/sparkle.svg",
            Self::Stack => "icons/stack.svg",
            Self::Terminal => "icons/terminal.svg",
            Self::Trash => "icons/trash.svg",
            Self::Unarchive => "icons/unarchive.svg",
            Self::Warning => "icons/warning.svg",
            Self::Worktree => "icons/worktree.svg",
        }
    }
}

/// A tintable SVG icon from the shared family.
#[derive(IntoElement)]
pub struct Icon {
    name: IconName,
    size: f32,
    color: Rgba,
}

impl Icon {
    pub const fn new(name: IconName, size: f32, color: Rgba) -> Self {
        Self { name, size, color }
    }
}

impl RenderOnce for Icon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        svg()
            .path(self.name.asset_path())
            .flex_none()
            .size(px(self.size))
            .text_color(self.color)
    }
}

/// Embedded SVG assets. The binary stays self-contained, so a packaged app
/// cannot lose its icons to a missing resource directory.
#[derive(Clone, Copy, Debug, Default)]
pub struct IconAssets;

impl AssetSource for IconAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(embedded_svg(path).map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        if path == "icons" || path == "icons/" {
            Ok(IconName::ALL
                .into_iter()
                .map(|icon| icon.asset_path().into())
                .collect())
        } else {
            Ok(Vec::new())
        }
    }
}

fn embedded_svg(path: &str) -> Option<&'static [u8]> {
    Some(match path {
        "icons/activity.svg" => include_bytes!("../assets/icons/activity.svg"),
        "icons/archive.svg" => include_bytes!("../assets/icons/archive.svg"),
        "icons/arrow-down.svg" => include_bytes!("../assets/icons/arrow-down.svg"),
        "icons/arrow-left.svg" => include_bytes!("../assets/icons/arrow-left.svg"),
        "icons/branch.svg" => include_bytes!("../assets/icons/branch.svg"),
        "icons/brand-claude.svg" => include_bytes!("../assets/icons/brand-claude.svg"),
        "icons/brand-copilot.svg" => include_bytes!("../assets/icons/brand-copilot.svg"),
        "icons/brand-cursor.svg" => include_bytes!("../assets/icons/brand-cursor.svg"),
        "icons/brand-gemini.svg" => include_bytes!("../assets/icons/brand-gemini.svg"),
        "icons/brand-kimi.svg" => include_bytes!("../assets/icons/brand-kimi.svg"),
        "icons/brand-openai.svg" => include_bytes!("../assets/icons/brand-openai.svg"),
        "icons/brand-opencode.svg" => include_bytes!("../assets/icons/brand-opencode.svg"),
        "icons/brand-pi.svg" => include_bytes!("../assets/icons/brand-pi.svg"),
        "icons/check.svg" => include_bytes!("../assets/icons/check.svg"),
        "icons/check-circle.svg" => include_bytes!("../assets/icons/check-circle.svg"),
        "icons/checklist.svg" => include_bytes!("../assets/icons/checklist.svg"),
        "icons/chevron-down.svg" => include_bytes!("../assets/icons/chevron-down.svg"),
        "icons/chevron-right.svg" => include_bytes!("../assets/icons/chevron-right.svg"),
        "icons/chevron-up.svg" => include_bytes!("../assets/icons/chevron-up.svg"),
        "icons/chevron-up-down.svg" => include_bytes!("../assets/icons/chevron-up-down.svg"),
        "icons/clock.svg" => include_bytes!("../assets/icons/clock.svg"),
        "icons/close.svg" => include_bytes!("../assets/icons/close.svg"),
        "icons/close-circle.svg" => include_bytes!("../assets/icons/close-circle.svg"),
        "icons/code.svg" => include_bytes!("../assets/icons/code.svg"),
        "icons/comment.svg" => include_bytes!("../assets/icons/comment.svg"),
        "icons/cube.svg" => include_bytes!("../assets/icons/cube.svg"),
        "icons/download.svg" => include_bytes!("../assets/icons/download.svg"),
        "icons/external-link.svg" => include_bytes!("../assets/icons/external-link.svg"),
        "icons/folder.svg" => include_bytes!("../assets/icons/folder.svg"),
        "icons/grid.svg" => include_bytes!("../assets/icons/grid.svg"),
        "icons/local-agents.svg" => include_bytes!("../assets/icons/local-agents.svg"),
        "icons/merge.svg" => include_bytes!("../assets/icons/merge.svg"),
        "icons/monitor.svg" => include_bytes!("../assets/icons/monitor.svg"),
        "icons/moon.svg" => include_bytes!("../assets/icons/moon.svg"),
        "icons/more.svg" => include_bytes!("../assets/icons/more.svg"),
        "icons/network.svg" => include_bytes!("../assets/icons/network.svg"),
        "icons/new-agent.svg" => include_bytes!("../assets/icons/new-agent.svg"),
        "icons/plus.svg" => include_bytes!("../assets/icons/plus.svg"),
        "icons/pointer.svg" => include_bytes!("../assets/icons/pointer.svg"),
        "icons/power.svg" => include_bytes!("../assets/icons/power.svg"),
        "icons/pull-request.svg" => include_bytes!("../assets/icons/pull-request.svg"),
        "icons/refresh.svg" => include_bytes!("../assets/icons/refresh.svg"),
        "icons/resize-horizontal.svg" => include_bytes!("../assets/icons/resize-horizontal.svg"),
        "icons/search.svg" => include_bytes!("../assets/icons/search.svg"),
        "icons/server.svg" => include_bytes!("../assets/icons/server.svg"),
        "icons/settings.svg" => include_bytes!("../assets/icons/settings.svg"),
        "icons/sidebar.svg" => include_bytes!("../assets/icons/sidebar.svg"),
        "icons/sidebar-right.svg" => include_bytes!("../assets/icons/sidebar-right.svg"),
        "icons/sparkle.svg" => include_bytes!("../assets/icons/sparkle.svg"),
        "icons/stack.svg" => include_bytes!("../assets/icons/stack.svg"),
        "icons/terminal.svg" => include_bytes!("../assets/icons/terminal.svg"),
        "icons/trash.svg" => include_bytes!("../assets/icons/trash.svg"),
        "icons/unarchive.svg" => include_bytes!("../assets/icons/unarchive.svg"),
        "icons/warning.svg" => include_bytes!("../assets/icons/warning.svg"),
        "icons/worktree.svg" => include_bytes!("../assets/icons/worktree.svg"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing asset renders as nothing at all, which reads as a broken
    /// control rather than a missing file. Catch it here instead.
    #[test]
    fn every_icon_name_has_an_embedded_asset() {
        for icon in IconName::ALL {
            assert!(embedded_svg(icon.asset_path()).is_some(), "{icon:?}");
        }
    }

    #[test]
    fn the_asset_source_lists_exactly_the_embedded_family() {
        let listed = IconAssets.list("icons").unwrap();
        assert_eq!(listed.len(), IconName::ALL.len());
        for icon in IconName::ALL {
            assert!(listed.iter().any(|path| path == icon.asset_path()));
        }
        assert!(IconAssets.list("elsewhere").unwrap().is_empty());
    }

    /// The optical scale stays at three sizes, for the same reason the type
    /// scale does.
    #[test]
    fn the_optical_scale_has_three_sizes() {
        let sizes = [IconSize::COMPACT, IconSize::REGULAR, IconSize::DISPLAY];
        assert!(sizes.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
