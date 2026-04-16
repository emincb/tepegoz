# Tepegöz v2 — Mobile client (working vision)

**Status.** Working product discussion, NOT a spec. Captures the state of the v2 plan after the 2026-04-16 user + PM-Claude + CTO-Claude discussion. **Two load-bearing architectural questions remain open; no v2 engineering work starts until both resolve.** When v2 engineering eventually begins, individual decisions lift from this doc into `docs/DECISIONS.md` with the usual locking discipline.

**Authority.** This doc is the v2 single source of truth while v1.0 ships. Freeze it in place; edit as user feedback lands. Do not read it as commitments — read it as a captured snapshot of in-flight thinking.

**Relationship to v1.** v1 ships with Phases 1–6 + Phase 10 (release packaging). Phases 7, 8, 9 are v1.1 candidates. See `docs/DECISIONS.md` / `project_v1_scope_trim` memory for rationale. v2 work does not start until v1.0 is released and the user has lived in it long enough for daily-use gaps to surface.

---

## The core pitch (reframed from the original v2 doc)

**You're at dinner. A Claude Code agent running on one of your VMs hits a decision point and needs input. Your phone buzzes. You open the app, see the agent's question, voice-dictate a response, send. Phone back in your pocket. Agent continues.**

This saves hours per week for anyone social who goes out without their laptop.

**What the pitch is NOT:**
- NOT a "god view on your phone" — the user explicitly rejected this framing.
- NOT a yes/no approval dialog — the reframe that came out of the discussion is that **full message composition to a running agent is the primary interaction**, not binary approval. Voice-to-text on Android is the affordance that makes mobile typing tolerable for this use case.
- NOT a "second-screen while at the laptop" tool — user dismissed this as a use case. The phone app is for when you're AWAY from the laptop.

**Load-bearing use case:** you are social, you are out, agents are working, something needs a prompt. Mobile response closes the loop in seconds instead of forcing you home.

---

## Locked decisions (accepted by user 2026-04-16)

- **Android native**, Kotlin + Jetpack Compose. Not React Native, not Flutter. Native gives full control over notification actions and terminal rendering.
- **APK direct download**, NOT Play Store. Play Store approach rejected as "too much work" for a personal tool. Downloadable APK on a releases page is sufficient.
- **No iOS in v2.0.** Unbookmarked; the user is Android-only.
- **Tab-based mobile UI**, not a port of the desktop tiled god-view. Bottom nav with one tab per scope.
- **User owns the network perimeter.** No Tailscale (explicitly rejected — "pain in the ass by itself"). No VPN layer. No corporate firewall. Any firewall involved is "owned by the god."
- **No mTLS** — conscious security simplification for UX clarity. The pairing token disambiguates which daemon, does NOT provide authentication. User accepts the weaker posture explicitly. Keep the pairing token long enough (~32 chars) that a port-scanner can't brute-force it.
- **No relay for user data.** If FCM is used at all (see open question #2), it carries only the wakeup signal. Pane content, approval commands, and all interactive traffic flow direct phone↔daemon.
- **Phase 2d (Ports + Processes tabs) cut from v2.0.** Same logic as v1's scope trim: mobile inspection of ports/processes has weak use-case pull. Defer to v2.1 if daily use surfaces real need.
- **Panes tab is chat-like, not approval-only.** Persistent text input at the bottom of the pane detail view is primary. Voice-to-text (Android native, free). Quick-response buttons (`y`/`n`/`continue`/customizable) above the keyboard for simple responses. Notification banner on detected prompts routes to either one-tap Y/N (simple confirms) OR "open for full response" action (complex prompts).
- **Hard first-time setup is acceptable.** Every launch AFTER the first must be trivially smooth. The first-time pairing flow gets to be a one-time pain.

---

## Open architectural questions (BOTH must resolve before v2 Phase 2a starts)

### 1. Where does the daemon live? [LOAD-BEARING]

The user's "smooth connection from the gym" requirement exposes that **home-laptop daemon has unfixable failure modes**:

- **Dynamic IP flip** — DynDNS handles this if it's the only failure mode.
- **Laptop asleep** — no solution exists; wake-on-LAN from cellular doesn't work across NATs.
- **Home ISP uses CGNAT** — laptop has a private address shared with hundreds of other ISP customers, no port forward can fix it, daemon is fundamentally unreachable from outside the ISP. Increasingly common on modern residential ISPs, universal on mobile broadband.
- **Visiting friend's wifi blocks outbound port 7777** — uncommon but possible.

CGNAT is the unfixable one without a tunnel/relay. The user explicitly rejected Tailscale and any relay for data.

**CTO lean:** **daemon lives on a VM with a static public IP**, not on laptop. User has a VM fleet already. VM doesn't sleep, IP doesn't flip, no CGNAT, port forwarding is stable. Laptop's TUI connects to the VM daemon remotely (same client, different transport). Phone connects to the same VM daemon. This sidesteps every home-laptop failure mode.

**Tradeoff:** daemon is no longer "on your laptop." Changes v1's mental model subtly — laptop becomes a workstation that runs the TUI against a remote persistent daemon.

**Pending from user:**
- Home ISP: public IP or CGNAT? Check with `curl ifconfig.me` on laptop, then try to reach that IP on any port from cellular. If nothing answers, CGNAT.
- Is daemon-on-VM acceptable, or does daemon-on-laptop matter for some reason not yet surfaced?
- If VM: which VM hosts it? New one or an existing one?

**This decision shapes everything else in v2.** No WSS transport work, no pairing flow design, no app scaffolding starts until this resolves.

### 2. Notification wakeup mechanism

User flagged notifications as "no big deal, no big engineering time wasted." Implicit signal: whatever's simplest at build time.

**Android reality:** a persistent WSS connection to your daemon dies within minutes of the phone's screen turning off (Doze mode), unless the app runs a foreground service with a permanently-visible "Tepegöz is running" notification in the shade 24/7.

**Two paths:**

- **Path A — FCM wakeup.** Daemon posts a tiny "wake up" signal to Firebase Cloud Messaging when a pane awaits input. Android delivers even while the app is frozen. App wakes, opens WSS direct to the daemon, fetches state, fires local notification with Allow/Deny. Google sees "phone got pinged at time T," nothing about content. All real data flows direct phone↔daemon.
- **Path B — Foreground service.** App runs a persistent service keeping WSS alive. Android requires a visible notification so user knows it's running. No Google dependency. Battery cost moderate. Notification bar permanently shows an entry.

**CTO lean:** **Path A (FCM for wakeup only).** Matches the user's "clean and simple" UX principle. Path B's permanent notification bar entry directly violates "clean." FCM's role is only the wakeup ping — no user data flows through Google.

**Pending:** user dismissed this as not architecturally-interesting; CTO lean is default unless user explicitly objects at build time.

---

## CTO recommendations not yet explicitly accepted or rejected

- **Wire codec: JSON adapter at the WSS boundary on the daemon side.** TUI keeps rkyv over Unix socket (v1 unchanged). Mobile app speaks JSON over WSS. Daemon grows ~200 LOC of serde JSON codec for the WSS transport only. **Reasoning:** reimplementing rkyv in Kotlin is a multi-week trap — zero-copy archived pointers don't translate cleanly to JVM memory model. JSON in Kotlin is trivial (kotlinx.serialization / Moshi). JNI via cargo-ndk works but is heavier than needed for v2. The v2-doc's "same wire protocol" claim conflicts with its "re-implement codec in Kotlin" suggestion; a JSON adapter reconciles both by framing it as "same protocol semantics, different wire codec per transport."

- **Prompt-detection heuristic is load-bearing — invest in Phase 2b.** The approval loop IS the pitch. The heuristic will have failure modes — false positives (trust-eroding) AND false negatives (user thinks agent is done, agent is actually stuck, worst outcome). Concrete asks for Phase 2b:
  - **Regression suite**: 50+ real Claude Code session recordings fed through the detector offline. CI-enforced.
  - **"I missed a prompt" feedback button in the app**: sends last N lines of pane output back to a suggestion log for pattern-library tuning.
  - **v2.1 cooperative marker with Anthropic**: escape sequence Claude Code emits when awaiting user input. Requires agent-side cooperation — flag on roadmap, don't depend on it for v2.0.

---

## UX shape (user + CTO convergence 2026-04-16)

### Panes tab (primary)

- **List view**: every open pane as a card. Shows pane label (`ssh:staging`, `zsh`), last line of output, live indicator (pulses when pane is producing output).
- **Pane detail view**: **chat-like interface**. Scrolling agent output top; **persistent text input at the bottom, always visible**, not a floating-action-button. Send button sends keystrokes to the pane verbatim. Multi-line input. Paste from clipboard. **Voice-to-text button** (Android native) — critical affordance since mobile typing is painful and the whole point is voice-dictate-a-prompt.
- **Quick-response row** above keyboard: `y`, `n`, `yes`, `no`, `continue`, `try another approach`. User-customizable. For when response really is simple.
- **Detected-prompt banner** at top of detail view when a prompt is detected: shows the prompt text; offers either (a) one-tap `Yes`/`No` buttons for simple `[y/N]`-shaped confirms, or (b) "Open for full response" action that drops into the detail view with keyboard up, for complex prompts ("what should I try next?"). Heuristic decides which shape to render.

### Docker tab

- **List view**: all containers, same columns as desktop tile — name, image, status, uptime. Color-coded by state (running green / stopped gray / exited red). Host picker at top of tab, defaults to Local.
- **Container detail view**: two sub-tabs, **Info** (metadata, ports, mounts) and **Logs** (live log stream, scrollable, auto-follow toggle). Logs is second-most-common mobile use case after pane approval.
- **Actions**: swipe a container row to reveal Restart, Stop, Remove. Confirm-before-destructive same as desktop. **No Kill action on mobile v2.0** (fat-finger risk).

### Fleet tab

- **List view**: all SSH hosts, connection state glyph (`●`/`◐`/`○`/`⚠`), alias, hostname.
- **Host detail view**: connection state, last-connected timestamp, agent deployed status.
- **Action**: Reconnect button on disconnected/error hosts. Primary mobile action — "something's broken, tap to retry."

### Notifications

- **Pane-awaiting-input**: primary notification. Android inline actions (`Allow` / `Deny`) directly on the notification shade for simple confirms. Tap notification body for full response.
- **Fleet host transitions to error state**: secondary.
- **Docker container exits unexpectedly**: optional, user-configurable.

---

## Phase structure (rough; subject to open questions resolving)

- **Phase 2a** — WSS transport + pairing CLI. Daemon opens WSS listener on configurable port. `tepegoz pair` prints QR-encoded connection string + long-enough pairing token. Android app scaffolding (Kotlin + Compose + xterminal-ish lib). Connection screen, save/load connections, connect/disconnect lifecycle. **Acceptance**: app connects, receives a Status event, displays daemon uptime.
- **Phase 2b** — Panes tab. **Most work of any phase.** Chat-like detail view, keystroke forwarding, voice input wiring, prompt-detection heuristic + 50-session regression suite, notifications with inline actions, "I missed a prompt" feedback. **Acceptance**: real Claude Code agent hits a permission prompt on a daemon-host, phone receives notification, user taps Allow via notification shade, agent continues.
- **Phase 2c** — Docker + Fleet tabs. Container actions, logs streaming, host picker retargeting, Fleet reconnect button. **Acceptance**: user tails Docker logs and reconnects a Fleet host from the phone.
- **Phase 2d** — Multi-daemon switching, notification preferences, polish, v2.0 APK release on GitHub Releases.

**Cut from v2.0**: Ports + Processes tabs — defer to v2.1 per the "let daily use surface the real gaps" methodology.

---

## Non-scope (v2.0)

- iOS
- Relay infrastructure for user data (FCM wakeup is not a relay for data)
- Multi-user / teams / sharing
- Plugin SDK
- Full pane editing (vim, tmux, etc.) on mobile
- Play Store publication
- Cloud-hosted daemon
- Apple Watch / smartwatch companion anything

---

## Engineering considerations

- **Wire codec**: JSON via kotlinx.serialization or Moshi on the Android side (CTO recommendation above). NOT rkyv reimplementation. Daemon gains serde JSON derives for all WSS-reachable types and a second codec module. Invisible to TUI which keeps rkyv over Unix socket.
- **Terminal rendering**: xterminal Android library or custom Canvas view. Pane output is mostly read — full vt100 emulation is lower priority than desktop — but ANSI colors + cursor positioning need to render correctly because Claude Code's output depends on them.
- **App repository**: separate repo OR monorepo sibling `android/` directory. Tactical call at Phase 2a kickoff. My lean: sibling directory — keeps the wire protocol version, release cadence, and install-script packaging under one source tree.
- **Notification pattern library**: lives in `tepegoz-core`; extensible via config file (`~/.config/tepegoz/prompt_patterns.toml`) so user can add custom regex patterns without a tepegoz update. Ships with starter list covering Claude Code's current format.
- **Daemon location migration** (if user picks VM): `tepegoz daemon` runs on the VM; laptop runs `tepegoz tui --daemon <vm-alias>` which connects over WSS or SSH. v1's local Unix-socket path stays as a fallback for truly-local use.

---

## Appendix A: Original user v2 product doc (preserved verbatim, 2026-04-16)

> ### What v2 is
>
> A native Android app that connects to a running Tepegöz daemon and gives you full control of your fleet from your phone. The primary use case is unblocking Claude Code agents when you're away from your laptop — an agent hits a permission prompt, stalls, you get a notification, you open the app, you tap yes, the agent continues. Secondary use cases: checking docker logs, seeing what's running, restarting a container, opening a shell. All from your phone, wherever you are.
>
> v2 does not replicate the desktop god view on a small screen. It is a purpose-built mobile interface against the same daemon and the same wire protocol v1 already ships.
>
> ### Connection model
>
> **The daemon exposes a port.** When `tepegoz daemon` starts, it opens a WSS listener alongside the existing Unix socket. Same wire protocol (rkyv-framed envelopes), new transport. No protocol fork, no separate implementation. Everything the TUI can see, the app can see.
>
> **No relay, no VPN, no Tailscale.** The user owns their network. Direct connection only.
>
> Two supported cases:
>
> - **VM with public IP**: daemon starts, exposes port, app connects directly. Trivially works.
> - **Laptop at home**: user does a one-time port forward on their home router. Their router, their rules, one-time configuration.
>
> **First-launch pairing flow:**
> 1. Start `tepegoz daemon` on the host machine
> 2. Run `tepegoz pair` — daemon prints a short connection string (IP:port + a connection token, human-readable, e.g. `tpg://192.168.1.50:7777/a3f9`)
> 3. Open the app, enter the connection string or scan a QR code the CLI renders in the terminal
> 4. App saves the connection permanently
> 5. Every subsequent launch: app opens, connects automatically, no interaction required
>
> The connection token is not a security layer — it's just a pairing identifier so the app knows which daemon to talk to when you have multiple. Security is the user's network perimeter, which they own.
>
> **Multiple daemons**: the app supports saving multiple connections (laptop, VM-1, VM-2). A connections screen on first open lets you pick which one or set a default. Once a default is set, opening the app goes directly to that daemon's view.
>
> ### UI structure
>
> The app is tab-based. Each tab is a scope. You pick a tab, you're in that scope full-screen. No tiles, no split view, no desktop layout on a phone.
>
> **Tab bar (bottom navigation):**
> - Panes
> - Docker
> - Ports
> - Processes
> - Fleet
>
> Each tab shows a list view by default. Tap a row to go into it. Back button returns to the list. Simple drill-down navigation throughout.
>
> ### Panes tab
>
> This is the most important tab. Claude Code agents live here.
>
> **List view**: every open pane as a card. Card shows the pane label (e.g. `ssh:staging`, `zsh`), last line of output, and a live indicator if the pane is producing output. Panes producing output pulse visually so you can see at a glance which agents are active.
>
> **Pane detail view**: full-screen terminal output. Scrollable, live-updating. Read-only by default — you're watching. A floating action button (keyboard icon) drops a text input at the bottom for sending keystrokes when you need to type something.
>
> **The approval interaction**: when a pane is waiting for input (Claude Code permission prompt, a yes/no question, a `[y/N]` confirmation), the app detects the pattern and surfaces a banner at the top of the pane detail view: the prompt text, a **Yes** button, a **No** button. One tap sends the response. Banner dismisses. Agent continues.
>
> **Push notifications**: when a pane that was idle starts waiting for input, the app sends a local notification — "tepegöz: `ssh:staging` is waiting for your input." Tap the notification, go directly to that pane's detail view, approve or reject. This is the core loop.
>
> [... remaining sections: Docker tab, Ports tab, Processes tab, Fleet tab, Notifications, What v2 is not, Wire protocol additions, Phase breakdown, Engineering notes for the CTO. Preserved verbatim in source chat; condensed here to keep doc workable. Full original available in conversation transcript dated 2026-04-16 if needed.]

## Appendix B: CTO review summary + user's redirection

**CTO raised five initial concerns:**

1. **Battery + FCM tension.** User dismissed: "notifications are no big deal, no big engineering time wasted on it." → Resolved as "use simplest path at build time"; CTO lean is FCM-for-wakeup-only.

2. **rkyv-in-Kotlin is a trap.** Still open; CTO recommends JSON adapter at WSS boundary. User hasn't pushed back.

3. **Prompt-detection heuristic fragility.** Still open; CTO flagged as load-bearing and the thing that could lose trust if it false-negatives. User didn't address directly but implied acceptance of the "invest in regression suite + feedback loop" path.

4. **mTLS dropped from the v2 doc despite being in v1 DECISIONS.md.** User confirmed deliberate for UX simplicity: "no authentication, firewall, etc problems. clean and simple." Accepted; just keep pairing token long enough.

5. **Phase 2d cut.** Implicitly confirmed by user's use-case enumeration (panes, Docker logs only). Ports + Processes on mobile don't pull weight.

**User reframed the product after CTO questioning:**

- **Agents-overnight was the WRONG framing.** User is social, out often. Real pitch is phone-as-remote-for-claude-when-socially-out. Saves hours weekly.
- **Full-message composition is primary, not Y/N approval.** Voice-to-text is the affordance that makes mobile typing acceptable. The v2 doc's "floating keyboard button" framing was reframed as "persistent chat-like input at bottom."
- **Connection reachability is THE problem.** Not notification wakeup. Home laptop at gym with dynamic IP + possible CGNAT + possible sleep is unreliable. CTO countered with daemon-on-VM as architectural solution — pending user answer.

**Current state:** CTO has asked user three questions (home ISP shape, daemon-on-VM acceptance, which VM) to unblock decision #1. User has paused the v2 discussion to ship v1.0 first. Decision deferred to post-v1.0-release.
