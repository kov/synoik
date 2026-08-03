use std::sync::mpsc;
use std::thread;

use accesskit::{
    ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, Live, Node, NodeId, Role,
    Tree, TreeId, TreeUpdate,
};
use accesskit_unix::Adapter;
use calloop::LoopHandle;

use crate::layout::workspace::WorkspaceId;
use crate::niri::{KeyboardFocus, Niri, State};

const ID_ROOT: NodeId = NodeId(0);
const ID_ANNOUNCEMENT: NodeId = NodeId(1);
const ID_SCREENSHOT_UI: NodeId = NodeId(2);
const ID_EXIT_CONFIRM_DIALOG: NodeId = NodeId(3);
const ID_OVERVIEW: NodeId = NodeId(4);
const ID_SWITCHER: NodeId = NodeId(5);
const ID_RUN_DIALOG: NodeId = NodeId(6);
const ID_END_SESSION_DIALOG: NodeId = NodeId(7);

pub struct A11y {
    event_loop: LoopHandle<'static, State>,
    focus: NodeId,
    workspace_id: Option<WorkspaceId>,
    /// The switcher item last announced, so a selection that has not moved is not re-read.
    switcher_selection: Option<usize>,
    last_switcher_label: String,
    last_announcement: String,
    to_accesskit: Option<mpsc::SyncSender<TreeUpdate>>,
}

enum Msg {
    InitialTree,
    Deactivate,
    Action(ActionRequest),
}

impl A11y {
    pub fn new(event_loop: LoopHandle<'static, State>) -> Self {
        Self {
            event_loop,
            focus: ID_ROOT,
            workspace_id: None,
            switcher_selection: None,
            last_switcher_label: String::new(),
            last_announcement: String::new(),
            to_accesskit: None,
        }
    }

    pub fn start(&mut self) {
        let (tx, rx) = calloop::channel::channel();
        let (to_accesskit, from_main) = mpsc::sync_channel::<TreeUpdate>(8);

        // The adapter has a tendency to deadlock, so put it on a thread for now...
        let handler = Handler { tx };
        let res = thread::Builder::new()
            .name("AccessKit Adapter".to_owned())
            .spawn(move || {
                let mut adapter = Adapter::new(handler.clone(), handler.clone(), handler);
                while let Ok(tree) = from_main.recv() {
                    let is_focused = tree.focus != ID_ROOT;
                    adapter.update_if_active(move || tree);
                    adapter.update_window_focus_state(is_focused);
                }
            });

        match res {
            Ok(_handle) => {}
            Err(err) => {
                warn!("error spawning the AccessKit adapter thread: {err:?}");
                return;
            }
        }

        self.event_loop
            .insert_source(rx, |e, _, state| match e {
                calloop::channel::Event::Msg(msg) => state.niri.on_a11y_msg(msg),
                calloop::channel::Event::Closed => (),
            })
            .unwrap();

        self.to_accesskit = Some(to_accesskit);
    }

    fn update_tree(&mut self, tree: TreeUpdate) {
        trace!("updating tree: {tree:?}");
        self.focus = tree.focus;

        let Some(tx) = &mut self.to_accesskit else {
            return;
        };
        match tx.try_send(tree) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                warn!("AccessKit channel is full, it probably deadlocked; disconnecting");
                self.to_accesskit = None;
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                warn!("AccessKit channel disconnected");
                self.to_accesskit = None;
            }
        }
    }
}

impl Niri {
    pub fn refresh_a11y(&mut self) {
        if self.a11y.to_accesskit.is_none() {
            return;
        }

        let _span = tracy_client::span!("refresh_a11y");

        let mut announcement = None;
        let ws_id = self.layout.active_workspace().map(|ws| ws.id());
        if let Some(ws_id) = ws_id {
            if self.a11y.workspace_id != Some(ws_id) {
                let (_, idx, ws) = self
                    .layout
                    .workspaces()
                    .find(|(_, _, ws)| ws.id() == ws_id)
                    .unwrap();

                let mut buf = format!("Workspace {}", idx + 1);
                if let Some(name) = ws.name() {
                    buf.push(' ');
                    buf.push_str(name);
                }

                announcement = Some(buf);
            }
        }
        self.a11y.workspace_id = ws_id;

        let focus = self.a11y_focus();

        // Check if the switcher's selection moved.
        let mut update_switcher_selection = false;
        if focus == ID_SWITCHER {
            let current = self.switcher.selected();
            if self.a11y.switcher_selection != current {
                update_switcher_selection = true;
                self.a11y.switcher_selection = current;
            }
        } else {
            self.a11y.switcher_selection = None;
        }

        let update_focus = self.a11y.focus != focus;

        if !(announcement.is_some() || update_focus || update_switcher_selection) {
            return;
        }

        let mut nodes = Vec::new();

        if let Some(mut announcement) = announcement {
            // Work around having to change node value for it to get announced.
            if announcement == self.a11y.last_announcement {
                announcement.push(' ');
            }
            self.a11y.last_announcement = announcement.clone();

            let mut node = Node::new(Role::Label);
            node.set_value(announcement);
            node.set_live(Live::Polite);
            nodes.push((ID_ANNOUNCEMENT, node));
        }

        if focus == ID_SWITCHER {
            // Ideally this would be a Group with a child Button per item, but niri found two
            // problems with that shape and neither has an explanation: Alt-Tab would always start
            // by reading the group's own label instead of the selected item, and once the list
            // went empty Orca stopped reading any child button for the rest of the session. So
            // the node flips between Group and Button instead, which works.
            if update_switcher_selection {
                if let Some(label) = self.switcher.selected_label() {
                    let mut label = label.to_owned();
                    // Change the label on a match so two same-titled windows are both announced.
                    if self.a11y.last_switcher_label == label {
                        label.push(' ');
                    }
                    self.a11y.last_switcher_label = label;

                    let mut node = Node::new(Role::Button);
                    node.set_label(&*self.a11y.last_switcher_label);
                    nodes.push((ID_SWITCHER, node));
                } else {
                    // An open switcher is never empty (the last item going away ends the
                    // session), so this is the closing frame.
                    nodes.push((ID_SWITCHER, Node::new(Role::Group)));
                }
            }
        }

        let update = TreeUpdate {
            nodes,
            tree: None,
            tree_id: TreeId::ROOT,
            focus,
        };

        self.a11y.update_tree(update);
    }

    pub fn a11y_announce(&mut self, mut announcement: String) {
        if self.a11y.to_accesskit.is_none() {
            return;
        }

        let _span = tracy_client::span!("a11y_announce");

        // Work around having to change node value for it to get announced.
        if announcement == self.a11y.last_announcement {
            announcement.push(' ');
        }
        self.a11y.last_announcement = announcement.clone();

        let mut node = Node::new(Role::Label);
        node.set_value(announcement);
        node.set_live(Live::Polite);

        let update = TreeUpdate {
            nodes: vec![(ID_ANNOUNCEMENT, node)],
            tree: None,
            tree_id: TreeId::ROOT,
            focus: self.a11y.focus,
        };

        self.a11y.update_tree(update);
    }

    pub fn a11y_announce_hotkey_overlay(&mut self) {
        if self.a11y.to_accesskit.is_none() {
            return;
        }

        self.a11y_announce(self.hotkey_overlay.a11y_text());
    }

    fn a11y_focus(&self) -> NodeId {
        match self.keyboard_focus {
            KeyboardFocus::ScreenshotUi => ID_SCREENSHOT_UI,
            KeyboardFocus::ExitConfirmDialog => ID_EXIT_CONFIRM_DIALOG,
            KeyboardFocus::RunDialog => ID_RUN_DIALOG,
            KeyboardFocus::EndSessionDialog => ID_END_SESSION_DIALOG,
            KeyboardFocus::Overview => ID_OVERVIEW,
            KeyboardFocus::Switcher => ID_SWITCHER,
            _ => ID_ROOT,
        }
    }

    fn on_a11y_msg(&mut self, msg: Msg) {
        match msg {
            Msg::InitialTree => {
                let tree = self.a11y_build_full_tree();
                trace!("sending initial tree: {tree:?}");
                self.a11y.update_tree(tree);
            }
            Msg::Deactivate => {
                trace!("deactivate");
            }
            Msg::Action(request) => {
                trace!("request: {request:?}");
            }
        }
    }

    fn a11y_build_full_tree(&self) -> TreeUpdate {
        let mut node = Node::new(Role::Label);
        node.set_live(Live::Polite);

        let mut screenshot_ui = Node::new(Role::Group);
        screenshot_ui.set_label("Screenshot UI");

        let exit_confirm_dialog = crate::ui::exit_confirm_dialog::a11y_node();
        let run_dialog = crate::ui::run_dialog::a11y_node();
        let end_session_dialog = crate::ui::end_session_dialog::a11y_node();

        let mut overview = Node::new(Role::Group);
        overview.set_label("Overview");

        let mut switcher_node = Node::new(Role::Group);
        switcher_node.set_label("Window switcher");

        let mut root = Node::new(Role::Window);
        root.set_children(vec![
            ID_ANNOUNCEMENT,
            ID_SCREENSHOT_UI,
            ID_EXIT_CONFIRM_DIALOG,
            ID_RUN_DIALOG,
            ID_END_SESSION_DIALOG,
            ID_OVERVIEW,
            ID_SWITCHER,
        ]);

        let tree = Tree {
            root: ID_ROOT,
            toolkit_name: Some(String::from("niri")),
            toolkit_version: None,
        };

        let focus = self.a11y_focus();

        // NOTE: we don't fill in current MRU selection here to avoid duplicating code; it should
        // get updated right away anyway.

        TreeUpdate {
            nodes: vec![
                (ID_ROOT, root),
                (ID_ANNOUNCEMENT, node),
                (ID_SCREENSHOT_UI, screenshot_ui),
                (ID_EXIT_CONFIRM_DIALOG, exit_confirm_dialog),
                (ID_RUN_DIALOG, run_dialog),
                (ID_END_SESSION_DIALOG, end_session_dialog),
                (ID_OVERVIEW, overview),
                (ID_SWITCHER, switcher_node),
            ],
            tree: Some(tree),
            tree_id: TreeId::ROOT,
            focus,
        }
    }
}

#[derive(Clone)]
struct Handler {
    tx: calloop::channel::Sender<Msg>,
}

impl ActivationHandler for Handler {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        let _ = self.tx.send(Msg::InitialTree);
        None
    }
}

impl DeactivationHandler for Handler {
    fn deactivate_accessibility(&mut self) {
        let _ = self.tx.send(Msg::Deactivate);
    }
}

impl ActionHandler for Handler {
    fn do_action(&mut self, request: ActionRequest) {
        let _ = self.tx.send(Msg::Action(request));
    }
}
