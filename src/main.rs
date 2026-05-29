mod downloader;
mod manifest;
mod strings;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, TryRecvError},
    time::Duration,
};

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use downloader::{DownloadEvent, DownloadPlan, start_downloads};
use manifest::{WallpaperAsset, WallpaperCatalog, load_manifest};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Wrap},
};
use strings::load_strings;
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(author, version, about = "Apple wallpaper manager TUI")]
struct Cli {
    /// Optional path to a manifest file or directory.
    #[arg(long)]
    manifest: Option<PathBuf>,

    /// Directory where downloads should be saved.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Number of concurrent download workers.
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(usize))]
    threads: usize,
}

#[derive(Debug, Error)]
enum AppError {
    #[error("{0}")]
    Manifest(#[from] manifest::ManifestError),
    #[error("{0}")]
    Strings(#[from] strings::StringsError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("manifest file was not found. tried: {0}")]
    ManifestNotFound(String),
    #[error("strings file was not found. tried: {0}")]
    StringsNotFound(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Browsing,
    Downloading,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionState {
    Unselected,
    Partial,
    Selected,
}

#[derive(Debug, Clone)]
enum TreeNodeKind {
    Category(usize),
    Subcategory {
        category: usize,
        subcategory: usize,
    },
    Asset {
        category: usize,
        subcategory: usize,
        asset: usize,
    },
}

#[derive(Debug, Clone)]
struct TreeNode {
    depth: usize,
    label: String,
    count: usize,
    kind: TreeNodeKind,
    expanded: bool,
}

#[derive(Debug)]
struct DownloadStatus {
    total_items: usize,
    completed_items: usize,
    failed_items: usize,
    active_jobs: BTreeMap<usize, ActiveDownloadStatus>,
    last_message: String,
    thread_count: usize,
    complete: bool,
}

#[derive(Debug, Clone)]
struct ActiveDownloadStatus {
    title: String,
    file_name: String,
    bytes_downloaded: u64,
    total_bytes: Option<u64>,
}

struct App {
    catalog: WallpaperCatalog,
    expanded_categories: BTreeSet<usize>,
    expanded_subcategories: BTreeSet<(usize, usize)>,
    selected_subcategories: BTreeSet<(usize, usize)>,
    selected_assets: BTreeSet<(usize, usize, usize)>,
    selected_index: usize,
    mode: Mode,
    status: Option<DownloadStatus>,
    log_lines: Vec<String>,
    output_dir: PathBuf,
    download_threads: usize,
}

impl App {
    fn new(catalog: WallpaperCatalog, output_dir: PathBuf, download_threads: usize) -> Self {
        Self {
            catalog,
            expanded_categories: BTreeSet::new(),
            expanded_subcategories: BTreeSet::new(),
            selected_subcategories: BTreeSet::new(),
            selected_assets: BTreeSet::new(),
            selected_index: 0,
            mode: Mode::Browsing,
            status: None,
            log_lines: vec!["Ready to download.".to_owned()],
            output_dir,
            download_threads,
        }
    }

    fn visible_nodes(&self) -> Vec<TreeNode> {
        let mut nodes = Vec::new();
        for (cat_idx, category) in self.catalog.categories.iter().enumerate() {
            let expanded = self.expanded_categories.contains(&cat_idx);
            let category_count = category
                .subcategories
                .iter()
                .map(|sub| sub.assets.len())
                .sum();
            nodes.push(TreeNode {
                depth: 0,
                label: category.name.clone(),
                count: category_count,
                kind: TreeNodeKind::Category(cat_idx),
                expanded,
            });
            if expanded {
                for (sub_idx, subcategory) in category.subcategories.iter().enumerate() {
                    let sub_expanded = self.expanded_subcategories.contains(&(cat_idx, sub_idx));
                    nodes.push(TreeNode {
                        depth: 1,
                        label: subcategory.name.clone(),
                        count: subcategory.assets.len(),
                        kind: TreeNodeKind::Subcategory {
                            category: cat_idx,
                            subcategory: sub_idx,
                        },
                        expanded: sub_expanded,
                    });
                    if sub_expanded {
                        for (asset_idx, asset) in subcategory.assets.iter().enumerate() {
                            nodes.push(TreeNode {
                                depth: 2,
                                label: asset.title.clone(),
                                count: 1,
                                kind: TreeNodeKind::Asset {
                                    category: cat_idx,
                                    subcategory: sub_idx,
                                    asset: asset_idx,
                                },
                                expanded: false,
                            });
                        }
                    }
                }
            }
        }
        nodes
    }

    fn current_node(&self) -> Option<TreeNode> {
        self.visible_nodes().get(self.selected_index).cloned()
    }

    fn selected_assets(&self) -> Vec<WallpaperAsset> {
        let mut assets = Vec::new();
        let mut seen = BTreeSet::new();

        for (cat_idx, category) in self.catalog.categories.iter().enumerate() {
            for (sub_idx, subcategory) in category.subcategories.iter().enumerate() {
                if self.selected_subcategories.contains(&(cat_idx, sub_idx)) {
                    for asset in &subcategory.assets {
                        if seen.insert(asset.file_name.clone()) {
                            assets.push(asset.clone());
                        }
                    }
                }
                for (asset_idx, asset) in subcategory.assets.iter().enumerate() {
                    if self
                        .selected_assets
                        .contains(&(cat_idx, sub_idx, asset_idx))
                        && seen.insert(asset.file_name.clone())
                    {
                        assets.push(asset.clone());
                    }
                }
            }
        }
        assets
    }

    fn subcategory_indices_for_category(&self, category: usize) -> Vec<(usize, usize)> {
        self.catalog
            .categories
            .get(category)
            .map(|cat| {
                cat.subcategories
                    .iter()
                    .enumerate()
                    .map(|(sub_idx, _)| (category, sub_idx))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn selection_state_for_category(&self, category: usize) -> SelectionState {
        let Some(cat) = self.catalog.categories.get(category) else {
            return SelectionState::Unselected;
        };
        if cat.subcategories.is_empty() {
            return SelectionState::Unselected;
        }

        let selected = cat
            .subcategories
            .iter()
            .enumerate()
            .filter(|(sub_idx, _)| {
                self.selection_state_for_subcategory(category, *sub_idx) == SelectionState::Selected
            })
            .count();
        if selected == 0 {
            SelectionState::Unselected
        } else if selected == cat.subcategories.len() {
            SelectionState::Selected
        } else {
            SelectionState::Partial
        }
    }

    fn selection_state_for_subcategory(
        &self,
        category: usize,
        subcategory: usize,
    ) -> SelectionState {
        if self
            .selected_subcategories
            .contains(&(category, subcategory))
        {
            SelectionState::Selected
        } else {
            let Some(sub) = self
                .catalog
                .categories
                .get(category)
                .and_then(|cat| cat.subcategories.get(subcategory))
            else {
                return SelectionState::Unselected;
            };

            if sub.assets.is_empty() {
                return SelectionState::Unselected;
            }

            let selected_assets = sub
                .assets
                .iter()
                .enumerate()
                .filter(|(asset_idx, _)| self.asset_is_selected(category, subcategory, *asset_idx))
                .count();

            if selected_assets == 0 {
                SelectionState::Unselected
            } else if selected_assets == sub.assets.len() {
                SelectionState::Selected
            } else {
                SelectionState::Partial
            }
        }
    }

    fn asset_exists(&self, category: usize, subcategory: usize, asset: usize) -> bool {
        self.catalog
            .categories
            .get(category)
            .and_then(|cat| cat.subcategories.get(subcategory))
            .and_then(|sub| sub.assets.get(asset))
            .map(|asset| self.output_dir.join(&asset.file_name).is_file())
            .unwrap_or(false)
    }

    fn subcategory_downloaded(&self, category: usize, subcategory: usize) -> bool {
        self.catalog
            .categories
            .get(category)
            .and_then(|cat| cat.subcategories.get(subcategory))
            .map(|sub| {
                !sub.assets.is_empty()
                    && sub
                        .assets
                        .iter()
                        .enumerate()
                        .all(|(asset_idx, _)| self.asset_exists(category, subcategory, asset_idx))
            })
            .unwrap_or(false)
    }

    fn category_downloaded(&self, category: usize) -> bool {
        self.catalog
            .categories
            .get(category)
            .map(|cat| {
                !cat.subcategories.is_empty()
                    && cat
                        .subcategories
                        .iter()
                        .enumerate()
                        .all(|(sub_idx, _)| self.subcategory_downloaded(category, sub_idx))
            })
            .unwrap_or(false)
    }

    fn selection_state_for_asset(
        &self,
        category: usize,
        subcategory: usize,
        asset: usize,
    ) -> SelectionState {
        if self.asset_is_selected(category, subcategory, asset) {
            SelectionState::Selected
        } else {
            SelectionState::Unselected
        }
    }

    fn asset_is_selected(&self, category: usize, subcategory: usize, asset: usize) -> bool {
        self.selected_assets
            .contains(&(category, subcategory, asset))
            || self
                .selected_subcategories
                .contains(&(category, subcategory))
    }

    fn toggle_current_selection(&mut self) {
        let Some(node) = self.current_node() else {
            return;
        };

        match node.kind {
            TreeNodeKind::Category(category) => {
                let keys = self.subcategory_indices_for_category(category);
                let all_selected = keys.iter().all(|(cat, subcategory)| {
                    self.selection_state_for_subcategory(*cat, *subcategory)
                        == SelectionState::Selected
                });
                if all_selected {
                    for key in keys {
                        self.selected_subcategories.remove(&key);
                        self.selected_assets
                            .retain(|(cat, sub, _)| (*cat, *sub) != key);
                    }
                } else {
                    for key in keys {
                        self.selected_subcategories.insert(key);
                    }
                }
            }
            TreeNodeKind::Subcategory {
                category,
                subcategory,
            } => {
                let key = (category, subcategory);
                if self.selected_subcategories.remove(&key) {
                    self.selected_assets
                        .retain(|(cat, sub, _)| (*cat, *sub) != key);
                } else {
                    self.selected_subcategories.insert(key);
                }
            }
            TreeNodeKind::Asset {
                category,
                subcategory,
                asset,
            } => {
                let key = (category, subcategory, asset);
                if !self.selected_assets.remove(&key) {
                    self.selected_assets.insert(key);
                }
            }
        }
    }

    fn expand_current(&mut self) {
        if let Some(TreeNode {
            kind: TreeNodeKind::Category(category),
            ..
        }) = self.current_node()
        {
            self.expanded_categories.insert(category);
        } else if let Some(TreeNode {
            kind:
                TreeNodeKind::Subcategory {
                    category,
                    subcategory,
                },
            ..
        }) = self.current_node()
        {
            self.expanded_subcategories.insert((category, subcategory));
        }
    }

    fn collapse_current(&mut self) {
        match self.current_node() {
            Some(TreeNode {
                kind: TreeNodeKind::Category(category),
                ..
            }) => {
                self.expanded_categories.remove(&category);
                self.expanded_subcategories
                    .retain(|(cat, _)| *cat != category);
            }
            Some(TreeNode {
                kind:
                    TreeNodeKind::Subcategory {
                        category,
                        subcategory,
                    },
                ..
            }) => {
                self.expanded_subcategories.remove(&(category, subcategory));
            }
            Some(TreeNode {
                kind: TreeNodeKind::Asset { .. },
                ..
            })
            | None => {}
        }
        let visible = self.visible_nodes();
        if self.selected_index >= visible.len() {
            self.selected_index = visible.len().saturating_sub(1);
        }
    }

    fn select_all(&mut self) {
        self.selected_subcategories.clear();
        for (cat_idx, category) in self.catalog.categories.iter().enumerate() {
            for (sub_idx, _) in category.subcategories.iter().enumerate() {
                self.selected_subcategories.insert((cat_idx, sub_idx));
            }
        }
        self.log("Selected all subcategories.");
    }

    fn clear_selection(&mut self) {
        self.selected_subcategories.clear();
        self.selected_assets.clear();
        self.log("Cleared selection.");
    }

    fn current_assets(&self) -> Vec<WallpaperAsset> {
        match self.current_node() {
            Some(TreeNode {
                kind: TreeNodeKind::Category(category),
                ..
            }) => self
                .catalog
                .categories
                .get(category)
                .into_iter()
                .flat_map(|cat| cat.subcategories.iter())
                .flat_map(|sub| sub.assets.iter().cloned())
                .collect(),
            Some(TreeNode {
                kind:
                    TreeNodeKind::Subcategory {
                        category,
                        subcategory,
                    },
                ..
            }) => self
                .catalog
                .categories
                .get(category)
                .and_then(|cat| cat.subcategories.get(subcategory))
                .map(|sub| sub.assets.clone())
                .unwrap_or_default(),
            Some(TreeNode {
                kind:
                    TreeNodeKind::Asset {
                        category,
                        subcategory,
                        asset,
                    },
                ..
            }) => self
                .catalog
                .categories
                .get(category)
                .and_then(|cat| cat.subcategories.get(subcategory))
                .and_then(|sub| sub.assets.get(asset))
                .cloned()
                .into_iter()
                .collect(),
            None => Vec::new(),
        }
    }

    fn remove_current_item(&mut self) -> Result<usize, String> {
        let assets = self.current_assets();
        if assets.is_empty() {
            return Err("Nothing selected to remove.".to_owned());
        }

        let mut removed = 0usize;
        for asset in assets {
            let path = self.output_dir.join(&asset.file_name);
            if path.is_file() {
                fs::remove_file(&path)
                    .map_err(|err| format!("Failed to remove {}: {}", path.display(), err))?;
                removed += 1;
                self.log(format!("Removed {}", path.display()));
            }
        }

        if removed == 0 {
            return Err("No downloaded files were found to remove.".to_owned());
        }

        Ok(removed)
    }

    fn move_up(&mut self) {
        let visible = self.visible_nodes();
        if visible.is_empty() {
            return;
        }
        if self.selected_index == 0 {
            self.selected_index = visible.len() - 1;
        } else {
            self.selected_index -= 1;
        }
    }

    fn move_down(&mut self) {
        let visible = self.visible_nodes();
        if visible.is_empty() {
            return;
        }
        self.selected_index = (self.selected_index + 1) % visible.len();
    }

    fn log(&mut self, message: impl Into<String>) {
        self.log_lines.push(message.into());
        if self.log_lines.len() > 6 {
            self.log_lines.remove(0);
        }
    }

    fn begin_downloads(&mut self) -> Result<mpsc::Receiver<DownloadEvent>, String> {
        let assets = self.selected_assets();
        if assets.is_empty() {
            return Err("No items selected.".to_owned());
        }
        let threads = self.download_threads.max(1);

        self.mode = Mode::Downloading;
        self.status = Some(DownloadStatus {
            total_items: assets.len(),
            completed_items: 0,
            failed_items: 0,
            active_jobs: BTreeMap::new(),
            last_message: format!(
                "Downloading {} files into {}",
                assets.len(),
                self.output_dir.display()
            ),
            thread_count: threads,
            complete: false,
        });
        self.log_lines.clear();
        self.log(format!(
            "Downloading {} assets with up to {} workers to {}",
            assets.len(),
            threads,
            self.output_dir.display()
        ));

        let (tx, rx) = mpsc::channel();
        let plan = DownloadPlan {
            assets,
            output_dir: self.output_dir.clone(),
            threads,
        };
        let _handle = start_downloads(plan, tx);
        Ok(rx)
    }

    fn handle_event(&mut self, event: DownloadEvent) {
        match event {
            DownloadEvent::Started {
                index,
                total,
                title,
                file_name,
                total_bytes,
            } => {
                if let Some(status) = self.status.as_mut() {
                    status.total_items = total;
                    status.active_jobs.insert(
                        index,
                        ActiveDownloadStatus {
                            title: title.clone(),
                            file_name: file_name.clone(),
                            bytes_downloaded: 0,
                            total_bytes,
                        },
                    );
                    status.last_message = format!("Writing {file_name}");
                }
                self.log(format!("Started {}/{}: {}", index + 1, total, title));
            }
            DownloadEvent::Progress {
                index,
                bytes_downloaded,
                total_bytes,
            } => {
                if let Some(status) = self.status.as_mut() {
                    if let Some(job) = status.active_jobs.get_mut(&index) {
                        job.bytes_downloaded = bytes_downloaded;
                        job.total_bytes = total_bytes;
                    }
                }
            }
            DownloadEvent::Finished { index, path } => {
                if let Some(status) = self.status.as_mut() {
                    status.active_jobs.remove(&index);
                    status.completed_items += 1;
                    status.last_message = format!("Saved {}", path.display());
                }
                self.log(format!("Saved {}", path.display()));
            }
            DownloadEvent::Skipped { index, path } => {
                if let Some(status) = self.status.as_mut() {
                    status.active_jobs.remove(&index);
                    status.completed_items += 1;
                    status.last_message = format!("Skipped {}", path.display());
                }
                self.log(format!("Skipped {}", path.display()));
            }
            DownloadEvent::Error { index, message } => {
                if let Some(status) = self.status.as_mut() {
                    status.active_jobs.remove(&index);
                    if index != usize::MAX {
                        status.completed_items += 1;
                        status.failed_items += 1;
                    }
                    status.last_message = message.clone();
                }
                self.log(format!("Error: {message}"));
            }
            DownloadEvent::Complete => {
                if let Some(status) = self.status.as_mut() {
                    status.complete = true;
                    status.last_message =
                        format!("Finished saving into {}", self.output_dir.display());
                }
                self.mode = Mode::Browsing;
                self.log("All downloads complete.");
            }
        }
    }
}

fn main() -> Result<(), AppError> {
    let cli = Cli::parse();
    let manifest_path = resolve_manifest_path(cli.manifest)?;
    let strings_path = resolve_strings_path(&manifest_path)?;
    let output_dir = cli
        .output
        .unwrap_or_else(|| default_output_dir(&manifest_path));

    let strings = load_strings(&strings_path)?;
    let catalog = load_manifest(&manifest_path, Some(&strings))?;
    run_app(catalog, output_dir, cli.threads)
}

fn run_app(
    catalog: WallpaperCatalog,
    output_dir: PathBuf,
    download_threads: usize,
) -> Result<(), AppError> {
    let mut terminal = setup_terminal()?;
    let mut app = App::new(catalog, output_dir, download_threads);
    let mut download_rx: Option<mpsc::Receiver<DownloadEvent>> = None;

    loop {
        terminal.draw(|frame| render_ui(frame, &app))?;

        if let Some(rx) = &download_rx {
            loop {
                match rx.try_recv() {
                    Ok(event) => {
                        let is_complete = matches!(event, DownloadEvent::Complete);
                        app.handle_event(event);
                        if is_complete {
                            download_rx = None;
                            break;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        download_rx = None;
                        break;
                    }
                }
            }
        }

        if event::poll(Duration::from_millis(60))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Up => app.move_up(),
                    KeyCode::Down => app.move_down(),
                    KeyCode::Right => app.expand_current(),
                    KeyCode::Left => app.collapse_current(),
                    KeyCode::Char(' ') => app.toggle_current_selection(),
                    KeyCode::Char('a') => app.select_all(),
                    KeyCode::Char('c') => app.clear_selection(),
                    KeyCode::Char('x') if app.mode == Mode::Browsing => {
                        match app.remove_current_item() {
                            Ok(removed) => app.log(format!("Removed {} files.", removed)),
                            Err(message) => app.log(message),
                        }
                    }
                    KeyCode::Enter | KeyCode::Char('d') if app.mode == Mode::Browsing => {
                        match app.begin_downloads() {
                            Ok(rx) => download_rx = Some(rx),
                            Err(message) => app.log(message),
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    teardown_terminal(&mut terminal)?;
    Ok(())
}

fn render_ui(frame: &mut ratatui::Frame<'_>, app: &App) {
    let footer_height = if app.status.is_some() { 14 } else { 10 };
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(footer_height),
        ])
        .split(frame.area());

    render_header(frame, outer[0], app);
    render_body(frame, outer[1], app);
    render_footer(frame, outer[2], app);
}

fn render_header(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let title = if app
        .status
        .as_ref()
        .map(|status| status.complete)
        .unwrap_or(false)
    {
        "awm complete"
    } else if matches!(app.mode, Mode::Downloading) {
        "awm downloading"
    } else {
        "awm"
    };

    let paragraph = Paragraph::new(Line::from(vec![
        Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::raw(format!(
            "{} categories  •  {} selected subcategories  •  output: {}",
            app.catalog.categories.len(),
            app.selected_subcategories.len(),
            app.output_dir.display()
        )),
    ]))
    .block(Block::default().borders(Borders::ALL));

    frame.render_widget(paragraph, area);
}

fn render_body(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(area);

    render_tree(frame, columns[0], app);
    render_details(frame, columns[1], app);
}

fn render_tree(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let nodes = app.visible_nodes();
    let items = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let selected = match node.kind {
                TreeNodeKind::Category(category) => app.selection_state_for_category(category),
                TreeNodeKind::Subcategory {
                    category,
                    subcategory,
                } => app.selection_state_for_subcategory(category, subcategory),
                TreeNodeKind::Asset {
                    category,
                    subcategory,
                    asset,
                } => app.selection_state_for_asset(category, subcategory, asset),
            };

            let marker = match selected {
                SelectionState::Unselected => "[ ]",
                SelectionState::Partial => "[-]",
                SelectionState::Selected => "[x]",
            };
            let expand_marker = match node.kind {
                TreeNodeKind::Category(_) => {
                    if node.expanded {
                        "▾"
                    } else {
                        "▸"
                    }
                }
                TreeNodeKind::Subcategory { .. } => {
                    if node.expanded {
                        "▾"
                    } else {
                        "▸"
                    }
                }
                TreeNodeKind::Asset { .. } => " ",
            };
            let prefix = "  ".repeat(node.depth);
            let display_label = match node.kind {
                TreeNodeKind::Asset {
                    category,
                    subcategory,
                    asset,
                } if app.asset_exists(category, subcategory, asset) => {
                    format!("{} (Downloaded)", node.label)
                }
                TreeNodeKind::Subcategory {
                    category,
                    subcategory,
                } if app.subcategory_downloaded(category, subcategory) => {
                    format!("{} (Downloaded)", node.label)
                }
                TreeNodeKind::Category(category) if app.category_downloaded(category) => {
                    format!("{} (Downloaded)", node.label)
                }
                _ => node.label.clone(),
            };
            let line = Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Yellow)),
                Span::raw(" "),
                Span::styled(expand_marker, Style::default().fg(Color::Cyan)),
                Span::raw(" "),
                Span::raw(prefix),
                Span::styled(
                    display_label,
                    if index == app.selected_index {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::raw(format!(" ({})", node.count)),
            ]);
            ListItem::new(line)
        })
        .collect::<Vec<_>>();

    let list = List::new(items)
        .block(Block::default().title("Categories").borders(Borders::ALL))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    let mut state = ListState::default();
    state.select(Some(app.selected_index));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_details(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let mut lines = Vec::new();
    if let Some(node) = app.current_node() {
        match node.kind {
            TreeNodeKind::Category(category) => {
                if let Some(cat) = app.catalog.categories.get(category) {
                    lines.push(Line::from(vec![
                        Span::styled("Category: ", Style::default().fg(Color::Yellow)),
                        Span::raw(cat.name.clone()),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled("ID: ", Style::default().fg(Color::Yellow)),
                        Span::raw(cat.id.clone()),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled("Subcategories: ", Style::default().fg(Color::Yellow)),
                        Span::raw(cat.subcategories.len().to_string()),
                    ]));
                    if let Some(description) = &cat.description {
                        lines.push(Line::from(vec![
                            Span::styled("Info: ", Style::default().fg(Color::Yellow)),
                            Span::raw(description.clone()),
                        ]));
                    }
                    lines.push(Line::from(" "));
                    lines.push(Line::from(Span::styled(
                        "Use Right/Left to expand or collapse",
                        Style::default().add_modifier(Modifier::BOLD),
                    )));
                    lines.push(Line::from(Span::raw(
                        "Space toggles selection for this category.",
                    )));
                }
            }
            TreeNodeKind::Subcategory {
                category,
                subcategory,
            } => {
                if let Some(sub) = app
                    .catalog
                    .categories
                    .get(category)
                    .and_then(|cat| cat.subcategories.get(subcategory))
                {
                    lines.push(Line::from(vec![
                        Span::styled("Subcategory: ", Style::default().fg(Color::Yellow)),
                        Span::raw(sub.name.clone()),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled("ID: ", Style::default().fg(Color::Yellow)),
                        Span::raw(sub.id.clone()),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled("Assets: ", Style::default().fg(Color::Yellow)),
                        Span::raw(sub.assets.len().to_string()),
                    ]));
                    let downloaded = sub
                        .assets
                        .iter()
                        .filter(|asset| app.output_dir.join(&asset.file_name).is_file())
                        .count();
                    lines.push(Line::from(vec![
                        Span::styled("Downloaded: ", Style::default().fg(Color::Yellow)),
                        Span::raw(format!("{} / {}", downloaded, sub.assets.len())),
                    ]));
                    if let Some(description) = &sub.description {
                        lines.push(Line::from(vec![
                            Span::styled("Info: ", Style::default().fg(Color::Yellow)),
                            Span::raw(description.clone()),
                        ]));
                    }
                    lines.push(Line::from(" "));
                    for asset in &sub.assets {
                        lines.push(Line::from(vec![
                            Span::raw(" - "),
                            Span::raw(format!(
                                "{} ({}; id: {}; {})",
                                asset.title, asset.file_name, asset.id, asset.extension
                            )),
                        ]));
                        if let Some(description) = &asset.description {
                            lines.push(Line::from(vec![
                                Span::raw("   "),
                                Span::styled("Info: ", Style::default().fg(Color::Yellow)),
                                Span::raw(description.clone()),
                            ]));
                        }
                    }
                }
            }
            TreeNodeKind::Asset {
                category,
                subcategory,
                asset: asset_idx,
            } => {
                if let Some(asset) = app
                    .catalog
                    .categories
                    .get(category)
                    .and_then(|cat| cat.subcategories.get(subcategory))
                    .and_then(|sub| sub.assets.get(asset_idx))
                {
                    lines.push(Line::from(vec![
                        Span::styled("Asset: ", Style::default().fg(Color::Yellow)),
                        Span::raw(asset.title.clone()),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled("File: ", Style::default().fg(Color::Yellow)),
                        Span::raw(asset.file_name.clone()),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled("Path: ", Style::default().fg(Color::Yellow)),
                        Span::raw(app.output_dir.join(&asset.file_name).display().to_string()),
                    ]));
                }
            }
        }
    }

    if let Some(status) = &app.status {
        lines.push(Line::from(" "));
        lines.push(Line::from(vec![
            Span::styled("Status: ", Style::default().fg(Color::Yellow)),
            Span::raw(&status.last_message),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Queue: ", Style::default().fg(Color::Yellow)),
            Span::raw(format!(
                "{} / {} complete",
                status.completed_items, status.total_items
            )),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Workers: ", Style::default().fg(Color::Yellow)),
            Span::raw(format!(
                "{} requested, {} active",
                status.thread_count,
                status.active_jobs.len()
            )),
        ]));
        if status.failed_items > 0 {
            lines.push(Line::from(vec![
                Span::styled("Errors: ", Style::default().fg(Color::Yellow)),
                Span::raw(status.failed_items.to_string()),
            ]));
        }
    }

    let block = Block::default().title("Details").borders(Borders::ALL);
    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn render_footer(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let active_height = app
        .status
        .as_ref()
        .map(|status| {
            if status.active_jobs.is_empty() {
                3
            } else if status.active_jobs.len() > 4 {
                7
            } else {
                (status.active_jobs.len().min(4) as u16) + 2
            }
        })
        .unwrap_or(3);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![
            Constraint::Length(active_height),
            Constraint::Length(3),
            Constraint::Min(2),
        ])
        .split(area);

    render_progress(frame, sections[0], app);
    render_overall_progress(frame, sections[1], app);
    render_log(frame, sections[2], app);
}

fn render_progress(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let block = Block::default()
        .title("Active Downloads")
        .borders(Borders::ALL);
    if let Some(status) = &app.status {
        if status.active_jobs.is_empty() {
            frame.render_widget(
                Paragraph::new("Waiting for workers to start.")
                    .block(block)
                    .wrap(Wrap { trim: true }),
                area,
            );
            return;
        }

        let mut lines = Vec::new();
        for (index, job) in status.active_jobs.iter().take(4) {
            let ratio = match job.total_bytes {
                Some(total) if total > 0 => (job.bytes_downloaded as f64 / total as f64).min(1.0),
                _ => 0.0,
            };
            let bar = progress_bar(ratio, 12);
            let bytes_label = match job.total_bytes {
                Some(total) => format_bytes(job.bytes_downloaded, total),
                None => format!("{} bytes", job.bytes_downloaded),
            };
            let label = format!(
                "#{} {}  {}  {}",
                index + 1,
                bar,
                bytes_label,
                truncate_label(&format!("{} ({})", job.title, job.file_name), 36)
            );
            lines.push(Line::from(label));
        }
        if status.active_jobs.len() > 4 {
            lines.push(Line::from(format!(
                "... and {} more active download(s)",
                status.active_jobs.len() - 4
            )));
        }

        let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
        frame.render_widget(paragraph, area);
    } else {
        frame.render_widget(
            Paragraph::new("No active download yet.")
                .block(block)
                .wrap(Wrap { trim: true }),
            area,
        );
    }
}

fn render_overall_progress(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let block = Block::default()
        .title("Queue Progress")
        .borders(Borders::ALL);
    if let Some(status) = &app.status {
        let ratio = if status.total_items == 0 {
            0.0
        } else {
            (status.completed_items as f64 / status.total_items as f64).min(1.0)
        };
        let label = format!("{} / {} files", status.completed_items, status.total_items);
        let gauge = Gauge::default()
            .block(block)
            .gauge_style(Style::default().fg(Color::Magenta))
            .ratio(ratio)
            .label(Span::raw(label));
        frame.render_widget(gauge, area);
    } else {
        frame.render_widget(
            Paragraph::new("Use Right/Left to expand/collapse, Space to select.")
                .block(block)
                .wrap(Wrap { trim: true }),
            area,
        );
    }
}

fn render_log(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let lines = app
        .log_lines
        .iter()
        .rev()
        .map(|entry| Line::from(entry.clone()))
        .collect::<Vec<_>>();

    let help = Line::from(vec![
        Span::styled("Keys: ", Style::default().fg(Color::Yellow)),
        Span::raw("↑↓ move  "),
        Span::raw("right expand  "),
        Span::raw("left collapse  "),
        Span::raw("space select  "),
        Span::raw("a all  "),
        Span::raw("c clear  "),
        Span::raw("x remove  "),
        Span::raw("d download  "),
        Span::raw("q quit"),
    ]);

    let mut display = vec![help];
    display.extend(lines);
    let paragraph = Paragraph::new(display)
        .block(Block::default().title("Messages").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn format_bytes(bytes_downloaded: u64, total_bytes: u64) -> String {
    format!(
        "{:.1} / {:.1} MiB",
        bytes_downloaded as f64 / 1024.0 / 1024.0,
        total_bytes as f64 / 1024.0 / 1024.0
    )
}

fn progress_bar(ratio: f64, width: usize) -> String {
    let filled = (ratio.clamp(0.0, 1.0) * width as f64).round() as usize;
    let filled = filled.min(width);
    let empty = width.saturating_sub(filled);
    format!("[{}{}]", "=".repeat(filled), " ".repeat(empty))
}

fn truncate_label(label: &str, max_len: usize) -> String {
    let mut iter = label.chars();
    let mut result = String::new();
    for _ in 0..max_len {
        if let Some(ch) = iter.next() {
            result.push(ch);
        } else {
            return result;
        }
    }

    if iter.next().is_some() {
        result.push_str("...");
    }
    result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<std::io::Stdout>>, AppError> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn teardown_terminal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> Result<(), AppError> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn resolve_manifest_path(explicit: Option<PathBuf>) -> Result<PathBuf, AppError> {
    if let Some(path) = explicit {
        return locate_manifest(&path)
            .ok_or_else(|| AppError::ManifestNotFound(path.display().to_string()));
    }

    let candidates = default_manifest_candidates();
    for candidate in &candidates {
        if let Some(found) = locate_manifest(candidate) {
            return Ok(found);
        }
    }

    Err(AppError::ManifestNotFound(
        candidates
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
    ))
}

fn resolve_strings_path(manifest_path: &Path) -> Result<PathBuf, AppError> {
    let candidates = strings_candidates_for_manifest(manifest_path);

    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }

    Err(AppError::StringsNotFound(
        candidates
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
    ))
}

fn locate_manifest(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }

    if path.is_dir() {
        let candidate = path.join("entries.json");
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    if let Some(parent) = path.parent() {
        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                let candidate = entry.path();
                if candidate
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("json"))
                    .unwrap_or(false)
                    && candidate
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(|name| name.contains("entries"))
                        .unwrap_or(false)
                {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

fn default_manifest_candidates() -> Vec<PathBuf> {
    let user_aerials_base = user_aerials_base();
    vec![
        user_aerials_base.join("manifest").join("entries.json"),
        legacy_idleassetsd_base()
            .join("Customer")
            .join("entries.json"),
    ]
}

fn manifest_base_dir(path: &Path) -> Option<PathBuf> {
    if path.is_dir() {
        return Some(path.to_path_buf());
    }
    let parent = path.parent()?;
    if parent.file_name().and_then(|name| name.to_str()) == Some("manifest") {
        parent.parent().map(|grandparent| grandparent.to_path_buf())
    } else {
        Some(parent.to_path_buf())
    }
}

fn strings_candidates_for_manifest(manifest_path: &Path) -> Vec<PathBuf> {
    if is_user_manifest(manifest_path) {
        let base = user_aerials_base();
        let bundle = base.join("manifest").join("TVIdleScreenStrings.bundle");
        return vec![
            bundle
                .join("Contents")
                .join("Resources")
                .join("Localizable.nocache.loctable"),
            bundle
                .join(format!("{}.lproj", current_locale_code()))
                .join("Localizable.nocache.strings"),
        ];
    }

    vec![
        legacy_idleassetsd_base()
            .join("Customer")
            .join("TVIdleScreenStrings.bundle")
            .join(format!("{}.lproj", current_locale_code()))
            .join("Localizable.nocache.strings"),
    ]
}

fn current_locale_code() -> String {
    if let Ok(lang) = std::env::var("APPLE_LOCALE") {
        let code = lang.trim();
        if code.len() >= 2 {
            return code.chars().take(2).collect::<String>();
        }
    }

    if let Ok(output) = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
    {
        if output.status.success() {
            if let Ok(locale) = String::from_utf8(output.stdout) {
                let code = locale.trim();
                if code.len() >= 2 {
                    return code.chars().take(2).collect::<String>();
                }
            }
        }
    }

    "en".to_owned()
}

fn default_output_dir(manifest_path: &Path) -> PathBuf {
    if is_user_manifest(manifest_path) {
        return user_aerials_base().join("videos");
    }
    legacy_idleassetsd_base()
        .join("Customer")
        .join("4KSDR240FPS")
}

fn user_aerials_base() -> PathBuf {
    home_join(&[
        "Library",
        "Application Support",
        "com.apple.wallpaper",
        "aerials",
    ])
}

fn legacy_idleassetsd_base() -> PathBuf {
    PathBuf::from("/Library/Application Support/com.apple.idleassetsd")
}

fn is_user_manifest(path: &Path) -> bool {
    let Some(base) = manifest_base_dir(path) else {
        return false;
    };
    base == user_aerials_base()
}

fn home_join(segments: &[&str]) -> PathBuf {
    let mut base = dirs::home_dir().unwrap_or_else(|| Path::new(".").to_path_buf());
    for segment in segments {
        base.push(segment);
    }
    base
}
