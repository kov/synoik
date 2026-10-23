// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The details behind a provider's icon in the overview search's "Still searching… / No answer /
//! Unavailable" row: what is wrong with it, what the bus said, its recent log, and what to do
//! about it. No GNOME counterpart — gnome-shell shows nothing for a provider that does not answer.
//!
//! Thin, like [`crate::ui::indicator_menu`]: [`crate::ui::widget::Menu`] does the box model,
//! hit-testing and painting. It opens with what the compositor knows and fills in when the search
//! worker has read the provider's log ([`Self::set_diagnosis`]) — that is a `journalctl` run, which
//! has no place on the compositor thread.
//!
//! The log is the provider's own output, so it is shown as text and nothing else.

use smithay::utils::{Logical, Point, Size};

use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::overview_search::LateState;
use crate::ui::popover::PopoverAction;
use crate::ui::widget::{Dir, Menu, MenuEntry, MenuHit, MenuItem};

/// The row ids.
const OPEN_APP: u64 = 1;
const HIDE: u64 = 2;

/// What the compositor knows when the menu opens.
#[derive(Debug, Clone)]
pub struct SearchProviderDetails {
    /// The provider's id — its app's desktop id.
    pub provider: String,
    pub name: String,
    pub state: LateState,
    /// What the bus or the call said, when it failed.
    pub error: Option<String>,
    /// Whether its app is one we can open.
    pub can_open: bool,
}

/// What the search worker found out afterwards.
pub use crate::search_providers::ProviderDiagnosis as Diagnosis;

pub struct SearchProviderMenu {
    details: SearchProviderDetails,
    /// `None` until the worker answers.
    diagnosis: Option<Diagnosis>,
    menu: Menu,
    max_height: Option<f64>,
}

impl SearchProviderMenu {
    pub fn new(details: SearchProviderDetails) -> Self {
        let mut menu = Self {
            details,
            diagnosis: None,
            menu: Menu::new(Vec::new()),
            max_height: None,
        };
        menu.rebuild();
        menu
    }

    pub fn provider(&self) -> &str {
        &self.details.provider
    }

    /// Whether a diagnosis is worth asking the worker for: there is nothing in the log of a
    /// provider that is merely not running or still searching.
    pub fn wants_diagnosis(state: LateState) -> bool {
        matches!(
            state,
            LateState::NoAnswer | LateState::Masked | LateState::Failed
        )
    }

    /// The worker's findings landed. Returns whether anything changed.
    pub fn set_diagnosis(&mut self, diagnosis: Diagnosis) -> bool {
        if self.diagnosis.as_ref() == Some(&diagnosis) {
            return false;
        }
        self.diagnosis = Some(diagnosis);
        self.rebuild();
        true
    }

    fn rebuild(&mut self) {
        let d = &self.details;
        let mut entries = vec![
            MenuEntry::SectionHeader(d.name.clone()),
            MenuEntry::Text {
                text: state_text(d.state).to_owned(),
                muted: false,
            },
        ];
        if let Some(error) = &d.error {
            entries.push(MenuEntry::Text {
                text: error.clone(),
                muted: true,
            });
        }
        if Self::wants_diagnosis(d.state) {
            match &self.diagnosis {
                None => entries.push(MenuEntry::Text {
                    text: "Reading its log…".to_owned(),
                    muted: true,
                }),
                Some(diagnosis) => {
                    if let Some(unit) = &diagnosis.unit {
                        entries.push(MenuEntry::Text {
                            text: format!("Service: {unit}"),
                            muted: true,
                        });
                    }
                    if diagnosis.log.is_empty() {
                        entries.push(MenuEntry::Text {
                            text: "Nothing in its log lately.".to_owned(),
                            muted: true,
                        });
                    } else {
                        entries.push(MenuEntry::SectionHeader("Recent log".to_owned()));
                        entries.extend(diagnosis.log.iter().map(|line| MenuEntry::Text {
                            text: line.clone(),
                            muted: true,
                        }));
                    }
                }
            }
        }
        entries.push(MenuEntry::Separator);
        if d.can_open {
            entries.push(MenuEntry::Item(MenuItem::new(OPEN_APP, "Open App")));
        }
        entries.push(MenuEntry::Item(MenuItem::new(HIDE, "Hide from Search")));
        self.menu.set_entries(entries);
        self.menu.set_max_height(self.max_height);
    }

    pub fn logical_size(&self) -> Size<f64, Logical> {
        self.menu.logical_size()
    }

    pub fn set_max_height(&mut self, max_height: Option<f64>) -> bool {
        self.max_height = max_height;
        self.menu.set_max_height(max_height)
    }

    pub fn scroll(&mut self, delta: f64) -> bool {
        self.menu.scroll_by(delta)
    }

    pub fn corner_radius(&self) -> f64 {
        self.menu.corner_radius()
    }

    /// The rows' labels, top to bottom. For the corpus.
    pub fn labels(&self) -> Vec<&str> {
        self.menu.labels()
    }

    /// The text paragraphs, top to bottom. For the corpus.
    pub fn texts(&self) -> Vec<&str> {
        self.menu.texts()
    }

    pub fn row_center(&self, label: &str) -> Option<Point<f64, Logical>> {
        self.menu.row_center(label)
    }

    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> PopoverAction {
        let hit = self.menu.pointer_click(pos);
        self.resolve(hit)
    }

    pub fn pointer_hover(&mut self, pos: Option<Point<f64, Logical>>) -> bool {
        self.menu.pointer_hover(pos)
    }

    pub fn focused_label(&self) -> Option<String> {
        self.menu.focused_label()
    }

    pub fn nav(&mut self, dir: Dir) -> bool {
        self.menu.nav(dir)
    }

    pub fn activate_focused(&mut self) -> PopoverAction {
        let hit = self.menu.activate_focused();
        self.resolve(hit)
    }

    fn resolve(&self, hit: MenuHit) -> PopoverAction {
        let provider = self.details.provider.clone();
        match hit {
            MenuHit::Activated(OPEN_APP) => PopoverAction::SearchProviderOpenApp(provider),
            MenuHit::Activated(HIDE) => PopoverAction::SearchProviderHide(provider),
            _ => PopoverAction::Consumed,
        }
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &crate::render_helpers::icon::IconCache,
        scale: f64,
        origin: Point<f64, Logical>,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        self.menu.render(
            renderer,
            icons,
            scale,
            origin,
            "a search provider's details",
        )
    }
}

/// The statement at the top: what is the matter, in a sentence.
fn state_text(state: LateState) -> &'static str {
    match state {
        LateState::Searching => "Still searching. Its results join the others when they come.",
        LateState::NoAnswer => "It did not answer in time.",
        LateState::NotRunning => "It is not running. Its results show while the app is open.",
        LateState::Masked => "Its service is masked, so it cannot be started.",
        LateState::Failed => "It failed to start, or to answer.",
    }
}
