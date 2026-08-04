# The polkit authentication agent

Ported 2026-08-02. Reference: `js/ui/components/polkitAgent.js` and
`src/shell-polkit-authentication-agent.c` (gnome-shell 50.3), plus polkit 127's own sources for the
wire protocols.

## Why it was a blocker

A session with no registered agent is not merely un-prompted. polkitd has nothing to ask with, so
**every action that needs authentication fails outright** — mounting an encrypted volume, enrolling
a fingerprint, installing updates, every Settings panel with a lock button. It was found by trying
to enrol a fingerprint as the test user (`net.reactivated.fprint.device.enroll` is
`auth_self_keep`) and it fails the same silent way everywhere else.

## Shape

| Piece | Where |
|---|---|
| Registration, the `AuthenticationAgent` interface, the helper conversation | `src/dbus/polkit_agent.rs` |
| What the dialog is doing | `src/polkit_dialog.rs` |
| Drawing, hit-testing, the open animation | `src/ui/polkit_dialog.rs` |
| Focus, input routing, render, the locked deferral | `src/synoik.rs`, `src/input/mod.rs` |

### The bus half

We export `org.freedesktop.PolicyKit1.AuthenticationAgent` on the system bus at polkit's default
path and hand polkitd our object with `RegisterAuthenticationAgent`. `BeginAuthentication` **does
not return until the user is done** — its reply *is* the answer, and `Cancelled` (not a failure) is
how the requesting program learns the user declined.

Only polkitd can call us: the shipped bus policy denies that interface to everyone else
(`/usr/share/dbus-1/system.d/org.freedesktop.PolicyKit1.conf`), which is why upstream does no caller
check either (`polkitagentlistener.c:287-289`). Note it grants the `polkitd` **user**, not uid 0 —
polkitd has been unprivileged since 121, so a hardcoded uid-0 check would reject the only legitimate
caller.

The subject is our logind session. polkitd refuses any other ("Passed session and the session the
caller is in differs", `polkitbackendinteractiveauthority.c:2521-2527`) and derives ours exactly the
way `freedesktop_login1::resolve_session_path` had to: `sd_pid_get_session`, then the user's
graphical session, because a shell started as a user service is outside the session scope
(`polkitbackendsessionmonitor-systemd.c:378-390`).

### The PAM half

GNOME gets this from `libpolkit-agent-1`'s `PolkitAgentSession`, which does nothing more exotic than
spawn the setuid `polkit-agent-helper-1` and speak a six-message line protocol to it. We spawn it
ourselves — linking the library would mean subclassing `PolkitAgentListener` from Rust and marrying
a `GMainContext` to calloop, for a protocol that fits on a page. The trust boundary is unchanged:
the helper is setuid root and runs PAM; we only carry text, and polkitd decides what the
authentication was worth.

```
→ helper   <cookie>\n on stdin        (argv would be world-readable — CVE-2015-4625)
           <response>\n               raw, not escaped
← helper   PAM_PROMPT_ECHO_OFF <text>
           PAM_PROMPT_ECHO_ON <text>
           PAM_ERROR_MSG <text>
           PAM_TEXT_INFO <text>
           SUCCESS | FAILURE
```

Cited at `polkitagentsession.c:468-506` (parse), `:599-631` (spawn), `:530-543` (respond),
`polkitagenthelper-pam.c:39-68` (the helper's side).

## Traps

**The payloads are `g_strescape`d, which escapes *bytes*.** A localised PAM prompt arrives as octal
escapes of its UTF-8, so unescaping per character turns `Senhã:` into `SenhÃ£:`. Nothing in an
English test notices. `unescape` works in bytes and decodes once at the end.

**The dialog must not appear when the request does.** GNOME builds it on `BeginAuthentication` but
only opens it from `_onSessionRequest` (`:297`). Opening earlier puts a modal grab on the seat with
nothing in it for as long as PAM takes to decide it wants anything — and PAM is free to take
seconds.

**A passwordless account must not have a conversation started for it.** For such an account,
*starting* one is the authentication (`:373-376`), so the dialog opens first and initiates only on
confirmation. Getting this backwards authorises the action with no prompt at all. It needs the
account's `PasswordMode`, which is resolved in the agent (a D-Bus round trip) and defaults to
"has a password" when AccountsService cannot answer.

**A refusal is not the end, and PAM's own error outranks ours.** GNOME explains, wiggles the entry
and starts another conversation (`:252-273`) — but only synthesises "Sorry, that didn't work" when
the error label is *empty*. PAM's own message names the actual problem (expired account, locked
account, a fingerprint that did not match); replacing it throws that away. An *info* message is not
an explanation and does not suppress it.

**Killing the helper is indistinguishable from a refusal** at the receiving end: both end as EOF ⇒
`Completed(false)`, and the dialog answers a refusal by starting another conversation. Every
conversation therefore carries an epoch, and events from a dead one are dropped.

**`Synoik::is_locked()` is not "the screen is covered".** It is only the `ext-session-lock` protocol's
state, so a screensaver-only shield (`lock-enabled = false`) reads as unlocked while covering
everything. `screen_is_covered()` is the one to gate on; the conformance test caught this on its
first run.

**The deferred request needs a resume that cannot be missed.** Two things cover the screen and only
one of them has an edge we own, so the resume is polled from `refresh_and_flush_clients` rather than
driven from an unlock. A held request nobody resumes is polkitd waiting forever.

## The three the tests could not have caught

All three were found live, in the isolated session described below, and none of them could fail in
the headless suite.

**Registration ran before the session path existed.** `polkit_agent::start` was placed above
`freedesktop_login1::start` in `dbus::start`, and login1 is what resolves `SESSION_OBJECT`. So
`session_path()` was always `None`, the agent logged "we have no logind session to register an agent
for" at debug and returned, and **it never registered on any machine** — the exact failure it exists
to fix. There is now a comment on the block saying why the order is load-bearing.

**The subject's details field is `a{sv}`, not `a(sv)`.** `Vec<(String, OwnedValue)>` serialises as
the latter, and polkitd rejects the whole call with `InvalidArgs: Type of message, ((sa(sv))ss),
does not match expected type`. It is a `HashMap`.

**The first element pushed is the topmost one.** The card is one opaque texture over the whole
dialog, so pushing it first drew the entry and the avatar *behind* it: live, the dialog came up
complete except for a blank gap where the password box belongs, and typing produced no bullets while
the state machine held the text perfectly. The bake-level test stayed green through all of it
because it baked the card by hand and never ran `render`. `the_entry_is_drawn_over_the_card` drives
the real `render` and asserts the push order.

## Live validation

Neither the seat (`gsrs`, session 4) nor `kov` may be touched, and polkitd allows one agent per
session — so the check runs in a *fresh* logind session for `gsrs`:

```
sudo systemd-run --uid=1002 --gid=1002 --property=PAMName=login --unit=gsrs-polkit-check --pipe \
    /bin/sh /tmp/livecheck.sh
```

Inside it, a private `dbus-launch` session bus and `XDG_RUNTIME_DIR=/tmp/gsrs-pk` keep the
compositor's well-known names and its polkit registration scoped away from the seat. The request
under test is `pkcheck --action-id org.freedesktop.hostname1.set-hostname --process $$
--allow-user-interaction`, chosen because its `allow_any` is `auth_admin_keep`: the first action
tried, `net.reactivated.fprint.device.enroll`, has `allow_any = no` and so can never prompt from an
isolated session, which reads exactly like a broken agent.

Confirmed end to end: registration, `BeginAuthentication`, a real setuid helper prompting, bullets
appearing as keys are injected, the avatar and the chosen administrator, and Escape producing
`polkit.dismissed=true` / "Authentication request was dismissed." out of `pkcheck` with the helper
reaped.

## Divergences

- **No identity chooser.** polkit may offer several administrators; GNOME picks one and logs that it
  did (`:51-61`). We pick the same way — ourselves, then `root`, then whoever is first — because
  asking for root's password when the user is themselves an admin is a prompt they may not be able
  to answer.
- **The action description gets two lines, always.** GNOME's box grows; ours reserves the room, for
  the same reason the caps and message rows are reserved. A longer message clips.
- **`%title_2` is drawn at weight 700, not 800** — the rasterizer's ceiling, a standing divergence
  across the whole port.
- **No insensitive button style.** GNOME makes Authenticate insensitive until there is text; we draw
  it unfocused instead of inventing a style the theme does not describe.

## Not done

- **Queued requests are answered one at a time but never shown as a queue.** Upstream is the same;
  noted because the scheduling code exists and looks like it should surface something.
- **`org.freedesktop.Malcontent.SessionLimits.Extend` is not exempt from the locked deferral**
  (`:442-443`). We have no time-limits subsystem, so the action cannot occur.
- **No click-to-place caret or selection in the entry** — the same MVP the lock screen's entry has.
