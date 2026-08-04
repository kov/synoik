// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

pub trait MergeWith<T> {
    fn merge_with(&mut self, part: &T);

    fn merged_with(mut self, part: &T) -> Self
    where
        Self: Sized,
    {
        self.merge_with(part);
        self
    }

    fn from_part(part: &T) -> Self
    where
        Self: Default + Sized,
    {
        Self::default().merged_with(part)
    }
}
