---
name: Bug report
about: Report a bug or a crash
title: ''
type: Bug
assignees: ''

---

<!-- Please describe the issue here at the top, then fill in the system information below. -->

<!--
Synoik has no configuration file: settings come from GSettings, the same keys GNOME uses. If the
bug involves a setting, the useful thing to attach is the output of the relevant schema, e.g.

$ gsettings list-recursively org.gnome.desktop.interface

If a behavior differs from GNOME Shell, please say which GNOME version you compared against —
"GNOME does X, synoik does Y" is the most actionable form a report can take.
-->

<!--
If the renderer is involved (glitches, corruption, a hang around drawing), running the session with
SYNOIK_VK_VALIDATION=1 and attaching the "VULKAN ERROR" lines from the journal is worth more than
any description.
-->

### System Information

<!-- Paste the output of `synoik -V`, e.g. synoik 26.4.0 (b94a5db) -->
* Synoik version: 

<!-- Write your distribution, e.g. Fedora 42 -->
* Distro: 

<!-- Write your GPU vendor and model, e.g. AMD RX 6700M -->
* GPU: 

<!-- Write your CPU vendor and model, e.g. AMD Ryzen 7 6800H -->
* CPU:
