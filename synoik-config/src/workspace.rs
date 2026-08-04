// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use crate::LayoutPart;

#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    pub name: WorkspaceName,
    pub open_on_output: Option<String>,
    pub layout: Option<WorkspaceLayoutPart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceName(pub String);

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceLayoutPart(pub LayoutPart);
