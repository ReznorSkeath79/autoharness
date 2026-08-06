//! A read-only view of the files a run works on.
//!
//! Pure model: walking a directory into rows, and reading one file with the
//! guards a UI must not forget — path confinement, a size ceiling, and a
//! binary check. Nothing here writes, and nothing follows a path outside the
//! root it was given: the worktree (or project) is the whole world.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Listing caps. A generated `node_modules` must not freeze the UI; when a
/// cap bites, the tree says so instead of quietly stopping.
const MAX_ENTRIES: usize = 2_000;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_LINES: usize = 5_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileEntry {
    /// Path relative to the root; stable id for expansion and selection.
    pub relative: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub depth: usize,
    pub expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileTree {
    pub entries: Vec<FileEntry>,
    /// True when MAX_ENTRIES cut the walk short.
    pub truncated: bool,
}

/// Walk `root` into visible rows: expanded directories recurse, collapsed
/// ones do not. Directories first, then files, both alphabetical,
/// case-insensitive. `.git` is administrative, not content, and is skipped.
pub(crate) fn tree(root: &Path, expanded: &HashSet<PathBuf>) -> FileTree {
    let mut entries = Vec::new();
    let mut truncated = false;
    walk(
        root,
        Path::new(""),
        0,
        expanded,
        &mut entries,
        &mut truncated,
    );
    FileTree { entries, truncated }
}

fn walk(
    root: &Path,
    relative: &Path,
    depth: usize,
    expanded: &HashSet<PathBuf>,
    out: &mut Vec<FileEntry>,
    truncated: &mut bool,
) {
    let Ok(reader) = std::fs::read_dir(root.join(relative)) else {
        return;
    };
    let mut children: Vec<(bool, String)> = reader
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                return None;
            }
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            Some((is_dir, name))
        })
        .collect();
    children.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });

    for (is_dir, name) in children {
        if out.len() >= MAX_ENTRIES {
            *truncated = true;
            return;
        }
        let child_relative = relative.join(&name);
        let is_expanded = is_dir && expanded.contains(&child_relative);
        out.push(FileEntry {
            relative: child_relative.clone(),
            name,
            is_dir,
            depth,
            expanded: is_expanded,
        });
        if is_expanded {
            walk(root, &child_relative, depth + 1, expanded, out, truncated);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileBody {
    Text { lines: Vec<String>, clipped: bool },
    Binary { size_bytes: u64 },
    TooLarge { size_bytes: u64 },
    Unreadable(String),
}

/// Read one file under `root`, refusing anything that resolves outside it.
/// Symlinks are the attack here: `link -> /etc` inside a worktree must not
/// turn the viewer into a browser of the whole disk.
pub(crate) fn read_file(root: &Path, relative: &Path) -> FileBody {
    let candidate = root.join(relative);
    let (Ok(canonical_root), Ok(canonical)) = (
        std::fs::canonicalize(root),
        std::fs::canonicalize(&candidate),
    ) else {
        return FileBody::Unreadable("file cannot be opened".into());
    };
    if !canonical.starts_with(&canonical_root) {
        return FileBody::Unreadable("outside the worktree".into());
    }
    let size_bytes = match std::fs::metadata(&canonical) {
        Ok(metadata) => metadata.len(),
        Err(error) => return FileBody::Unreadable(error.to_string()),
    };
    if size_bytes > MAX_FILE_BYTES {
        return FileBody::TooLarge { size_bytes };
    }
    let bytes = match std::fs::read(&canonical) {
        Ok(bytes) => bytes,
        Err(error) => return FileBody::Unreadable(error.to_string()),
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return FileBody::Binary { size_bytes };
    };
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let clipped = lines.len() > MAX_LINES;
    lines.truncate(MAX_LINES);
    FileBody::Text { lines, clipped }
}

/// Where the viewer is rooted for the selected run: its worktree when one
/// exists on disk, else the project checkout. The label says which, so the
/// user always knows whose files they are reading.
pub(crate) fn viewer_root(state: &crate::client::UiState) -> Option<(PathBuf, &'static str)> {
    if let Some(path) = state
        .selected_detail()
        .and_then(|detail| detail.worktree.as_ref())
        .and_then(|worktree| worktree.path.clone())
    {
        let path = PathBuf::from(path);
        if path.is_dir() {
            return Some((path, "worktree"));
        }
    }
    let project = state.selected_project()?;
    let path = PathBuf::from(&project.path);
    path.is_dir().then_some((path, "repository"))
}

/// The Files tab of the right-hand inspector: tree up top, one file below.
/// Vertical because the pane is narrow — a side-by-side dialog layout would
/// crush the code against the tree. Read-only by construction — every byte
/// shown came through [`read_file`]'s guards.
pub(crate) fn inspector_view(
    expanded: &HashSet<PathBuf>,
    selected: Option<&Path>,
    body: Option<&(PathBuf, FileBody)>,
    state: &crate::client::UiState,
    cx: &mut gpui::Context<crate::Shell>,
) -> gpui::AnyElement {
    use crate::icon::{Icon, IconName, IconSize};
    use crate::theme;
    use crate::theme::{Colors, Fill, Space, Surface, Tone, Typo};
    use gpui::prelude::*;
    use gpui::{ElementId, Role, SharedString, div, px};

    let root = viewer_root(state);

    let content: gpui::AnyElement = match root {
        // The diri lesson: an empty state is a dead end unless it carries its
        // own way out. The button IS the instruction.
        None => div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.0))
            .p(px(24.0))
            .child(
                div()
                    .text_size(px(Typo::ROW.size))
                    .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                    .child("No folder open — add one and its files appear here."),
            )
            .child(add_folder_button(cx))
            .into_any_element(),
        Some((root_path, kind)) => {
            let root_label = div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(Space::INDENT))
                .py(px(4.0))
                .border_b_1()
                .border_color(Colors::stroke())
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(Typo::META.size))
                        .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                        .child(SharedString::from(format!(
                            "{kind} · {}",
                            root_path.to_string_lossy()
                        ))),
                )
                .child(add_folder_button(cx));
            let listing = tree(&root_path, expanded);
            let tree_pane = div()
                .id("files-tree")
                .flex_none()
                .max_h(px(280.0))
                .overflow_y_scroll()
                .py(px(4.0))
                .border_b_1()
                .border_color(Colors::stroke())
                .children(
                    listing
                        .entries
                        .into_iter()
                        .enumerate()
                        .map(|(index, entry)| {
                            let is_selected = selected == Some(entry.relative.as_path());
                            let toggle_path = entry.relative.clone();
                            let select_root = root_path.clone();
                            let select_path = entry.relative.clone();
                            let is_dir = entry.is_dir;
                            div()
                                .id(ElementId::Name(format!("files-entry-{index}").into()))
                                .role(Role::Button)
                                .aria_label(entry.relative.to_string_lossy().into_owned())
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .h(px(24.0))
                                .pl(px(8.0 + entry.depth as f32 * 12.0))
                                .pr(px(8.0))
                                .cursor_pointer()
                                .when(is_selected, |row| row.bg(Fill::selected(true)))
                                .hover(|row| row.bg(theme::white(Fill::HOVER)))
                                .on_click(cx.listener(move |shell, _, _, cx| {
                                    if is_dir {
                                        shell.files_toggle_dir(toggle_path.clone(), cx);
                                    } else {
                                        shell.files_select(
                                            select_root.clone(),
                                            select_path.clone(),
                                            cx,
                                        );
                                    }
                                }))
                                .child(Icon::new(
                                    if !entry.is_dir {
                                        IconName::Code
                                    } else if entry.expanded {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    },
                                    IconSize::COMPACT,
                                    Colors::text(Surface::Content, Tone::Tertiary),
                                ))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .truncate()
                                        .text_size(px(Typo::ROW.size))
                                        .text_color(Colors::text(
                                            Surface::Content,
                                            if entry.is_dir {
                                                Tone::Primary
                                            } else {
                                                Tone::Secondary
                                            },
                                        ))
                                        .child(SharedString::from(entry.name)),
                                )
                        }),
                )
                .when(listing.truncated, |pane| {
                    pane.child(
                        div()
                            .px(px(8.0))
                            .py(px(4.0))
                            .text_size(px(Typo::META.size))
                            .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                            .child(format!("stopped at {MAX_ENTRIES} entries")),
                    )
                });

            let viewer: gpui::AnyElement = match (selected, body) {
                (Some(path), Some((loaded, body))) if loaded == path => match body {
                    FileBody::Text { lines, clipped } => div()
                        .id("files-viewer")
                        .flex_1()
                        .min_w(px(0.0))
                        .h_full()
                        .overflow_y_scroll()
                        .py(px(4.0))
                        .children(lines.iter().enumerate().map(|(number, line)| {
                            div()
                                .flex()
                                .px(px(Space::INDENT))
                                .font_family("SF Mono")
                                .text_size(px(Typo::META_MONO.size))
                                .child(
                                    div()
                                        .flex_none()
                                        .w(px(40.0))
                                        .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                                        .child(SharedString::from(format!("{}", number + 1))),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .text_color(Colors::text(Surface::Content, Tone::Secondary))
                                        .child(SharedString::from(line.clone())),
                                )
                        }))
                        .when(*clipped, |viewer| {
                            viewer.child(
                                div()
                                    .px(px(Space::INDENT))
                                    .py(px(4.0))
                                    .text_size(px(Typo::META.size))
                                    .text_color(crate::theme::Ink::ATTENTION)
                                    .child(format!("clipped at {MAX_LINES} lines")),
                            )
                        })
                        .into_any_element(),
                    FileBody::Binary { size_bytes } => {
                        viewer_note(format!("Binary file · {size_bytes} bytes"))
                    }
                    FileBody::TooLarge { size_bytes } => viewer_note(format!(
                        "Too large to show · {size_bytes} bytes (limit {MAX_FILE_BYTES})"
                    )),
                    FileBody::Unreadable(reason) => viewer_note(reason.clone()),
                },
                _ => viewer_note("Select a file".into()),
            };

            div()
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .child(root_label)
                .child(tree_pane)
                .child(viewer)
                .into_any_element()
        }
    };

    div()
        .id("files-page")
        .role(Role::Region)
        .aria_label("File viewer")
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .child(content)
        .into_any_element()
}

/// The one way a folder enters this tab: the native directory picker, then
/// `project.add`, whose reply selects the repository — so adding IS opening.
fn add_folder_button(cx: &mut gpui::Context<crate::Shell>) -> gpui::AnyElement {
    use crate::components;
    use crate::theme;
    use crate::theme::Fill;
    use gpui::Role;
    use gpui::prelude::*;

    components::compact_control("Add folder…")
        .id("files-add-folder")
        .role(Role::Button)
        .aria_label("Add a folder as a repository and open it here")
        .hover(|button| button.bg(theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(|shell, _, window, cx| {
            shell.choose_repository(window, cx);
        }))
        .into_any_element()
}

fn viewer_note(text: String) -> gpui::AnyElement {
    use crate::theme::{Colors, Surface, Tone, Typo};
    use gpui::prelude::*;
    use gpui::{SharedString, div, px};
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .p(px(24.0))
        .text_size(px(Typo::ROW.size))
        .text_color(Colors::text(Surface::Content, Tone::Tertiary))
        .child(SharedString::from(text))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("README.md"), "hello\nworld\n").unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join(".git/config"), "[core]\n").unwrap();
        dir
    }

    #[test]
    fn the_tree_lists_dirs_first_skips_git_and_recurses_only_when_expanded() {
        let dir = fixture();

        let collapsed = tree(dir.path(), &HashSet::new());
        assert_eq!(
            collapsed
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["src", "README.md"],
            ".git never appears, collapsed dirs do not recurse"
        );

        let mut expanded = HashSet::new();
        expanded.insert(PathBuf::from("src"));
        let open = tree(dir.path(), &expanded);
        assert_eq!(
            open.entries
                .iter()
                .map(|entry| (entry.name.as_str(), entry.depth))
                .collect::<Vec<_>>(),
            vec![("src", 0), ("main.rs", 1), ("README.md", 0)]
        );
        assert!(!open.truncated);
    }

    #[test]
    fn reading_stays_inside_the_root() {
        let dir = fixture();

        match read_file(dir.path(), Path::new("README.md")) {
            FileBody::Text { lines, clipped } => {
                assert_eq!(lines, vec!["hello", "world"]);
                assert!(!clipped);
            }
            other => panic!("expected text, got {other:?}"),
        }

        // A traversal cannot leave the root (refused as unresolvable or as
        // outside — either way, never read), and a symlink out is refused.
        assert!(matches!(
            read_file(dir.path(), Path::new("../../etc/passwd")),
            FileBody::Unreadable(_)
        ));
        std::os::unix::fs::symlink("/etc", dir.path().join("escape")).unwrap();
        assert_eq!(
            read_file(dir.path(), Path::new("escape/passwd")),
            FileBody::Unreadable("outside the worktree".into())
        );
    }

    #[test]
    fn binary_and_oversize_files_are_named_not_rendered() {
        let dir = fixture();
        std::fs::write(dir.path().join("blob"), [0u8, 159, 146, 150]).unwrap();
        match read_file(dir.path(), Path::new("blob")) {
            FileBody::Binary { size_bytes } => assert_eq!(size_bytes, 4),
            other => panic!("expected binary, got {other:?}"),
        }

        let big = vec![b'a'; (MAX_FILE_BYTES + 1) as usize];
        std::fs::write(dir.path().join("big.txt"), big).unwrap();
        assert!(matches!(
            read_file(dir.path(), Path::new("big.txt")),
            FileBody::TooLarge { .. }
        ));
    }
}
