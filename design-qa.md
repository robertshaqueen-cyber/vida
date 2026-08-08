# UI framework design QA

- Source visual truth: `https://github.com/wilsonglasser/oryxis/blob/main/resources/screen_1.png`
- Secondary source state: `https://github.com/wilsonglasser/oryxis/blob/main/resources/screen_7.png`
- Implementation: Vida native iced application on `codex/ui-framework-overhaul`
- Intended viewport: 1024 × 768 logical pixels, scale factor 1
- Source pixels: 1200 × 750, scale factor 1
- Implementation screenshot: `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-9c3556ae-e8f8-4682-85a2-15042fd2cad7.png`
- Post-fix screenshot: `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-a92b8b49-2648-40d8-b97c-e6bb93d2557b.png`
- Unlock screenshots: `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-9d095c1b-086c-430c-b4cc-6a2b6d3e5b61.png`, `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-619e7eb6-e654-4e2c-b37c-58956b7c0f1b.png`, `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-06a0933e-7cc6-4e31-82ec-29cc0983169c.png`
- Host editor screenshots: `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-c0f8788e-1ade-4749-bec2-3870dbd2b8f8.png`, `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-d532526a-7d67-444d-8a10-141c678937d3.png`
- Quick-connect screenshot: `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-ba96180e-afa6-405c-924b-cd43146ee0d4.png`
- Settings screenshots: `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-efd7d94f-5af8-43f4-8659-006e2b79844f.png`, `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-f48cdea0-6a39-489c-854c-6764f6c7ee70.png`
- Settings consistency screenshots: `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-e7383f9d-483b-4e08-9ebd-5f191ac6648a.png`, `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-559ed6e1-6936-415f-a5cc-32dbfaec40fd.png`, `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-2074d613-b120-4c31-9199-c86bd133fdcf.png`
- Legacy backup screenshot: `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-dfccfdc8-4b1f-40f4-8cf8-9ee28e904bd5.png`
- Misleading backup success screenshot: `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-8f7d18f9-1596-42c4-b97c-4570b4aad7d0.png`
- Sync button comparison screenshot: `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-fa57a2e7-49f1-41db-9ff9-20605f72a77f.png`
- Backup/restore button reference screenshot: `/var/folders/b1/8lj_4xm94b5gzl2zmscbph1r0000gn/T/codex-clipboard-ed020f4e-4742-4e6c-88a0-f5425fcbeaf5.png`
- Implementation pixels: 1022 × 802 including the native title bar, scale factor 1
- Post-fix capture: 1920 × 1080 desktop; Vida window remains approximately 1024 × 768,
  scale factor 1
- State: connection failure

## Full-view comparison evidence

The source screenshots and the owner's first Vida capture were opened and inspected together.
The Vida connection-failure state does not exist in the Oryxis source set, so exact content matching
is not possible; the comparison is limited to desktop density, surface proportions, typography,
color tokens, icon fidelity, and layout rhythm.

## Focused region comparison evidence

The connection card was inspected at full resolution. The terminal badge occupied almost the entire
card width and more than 500 px of height, while its intended slot was 54 × 54 px. Main-toolbar and
host-header badges used the same sizing pattern and were checked in code for the same defect.
The post-fix capture confirms a 54 px badge inside a compact horizontal dialog; no focused crop is
needed because the title, icon, explanatory copy, and primary action are all legible in the full view.

## Findings

- [Resolved P1] Fixed icon badge expanded to fill its parent.
  - Location: connection state; same API pattern in unlock, top bar, and host header.
  - Evidence: the 54 px terminal badge rendered approximately 356 × 538 px, producing a portrait,
    mobile-card composition inside a desktop window.
  - Impact: the first screen reads as a mobile mockup and hides the intended compact desktop density.
  - Fix applied: replace fixed `width/height` followed by `center_x/y(Length::Fill)` with
    `center_x/y(<fixed size>)`; change connection failure to a 600 px horizontal dialog and unlock
    to a 520 px desktop dialog with a horizontal identity header.
  - Post-fix evidence: the new capture shows desktop proportions, a correctly sized icon, balanced
    whitespace, readable typography, consistent dark/teal tokens, a sharp Lucide glyph, and unchanged
    application copy. No actionable P0/P1/P2 issue remains in the connection-failure state.
- [P1] Revised implementation evidence is still required.
  - Location: connection, main host view, and settings view.
  - Evidence: the owner's screenshot proves the original issue, but a post-fix capture is not yet
    available.
  - Impact: the corrected proportions and icon baseline cannot yet be signed off visually.
  - Fix: capture the rebuilt connection state and the two main application states.
- [P2] Unlock error treatment uses the old visual language.
  - Location: unlock password input and inline error notice.
  - Evidence: normal and focused states use subtle one-pixel teal borders and layered green-black
    surfaces, while the error state switches to a two-pixel saturated red input border plus a nearly
    black rectangle with a bright red outline and no status icon.
  - Impact: the error looks pasted on from a different component system and draws more attention to
    its border than to the recovery message.
  - Fix applied: reduce the input error border to one pixel at 72% opacity; add a shared error-notice
    surface with 10% danger tint, 28% border, softer danger text, and a Lucide alert icon.
  - Post-fix evidence: pending owner capture.
- [Resolved P1 in code; capture pending] Connection host name collapses into a vertical column.
  - Location: Settings → Connections host list.
  - Evidence: the two-character host name is rendered one character per line while most of the card
    remains empty.
  - Impact: host identity becomes hard to scan and the layout appears broken.
  - Fix applied: replace the unstructured text button with a full-width horizontal host row containing
    a fixed server badge and a fill-width two-line identity column; make both list and section columns
    explicitly fill the card width.
- [Resolved P2 in code; capture pending] Quick-connect panel has duplicate chrome and duplicate plus.
  - Location: quick-connect overlay and its primary action.
  - Evidence: the panel shows two nested rounded borders, and the action reads `+ +新增主机`.
  - Impact: the overlay looks heavier than other surfaces and the main action contains a visible copy error.
  - Fix applied: keep only the outer elevated panel surface; remove the leading plus from translated copy
    because the button already uses the Lucide plus icon.
- [Resolved P2 in code; capture pending] Add/edit host screens remain on the legacy form treatment.
  - Location: New Host and Edit Host tabs.
  - Evidence: inputs and actions float directly on the page while Settings uses a bordered surface,
    shared input states, and structured headers.
  - Impact: adjacent configuration workflows look as though they belong to different applications.
  - Fix applied: move both forms into the shared surface, add a compact icon-and-title header, apply
    shared input/button/error styles, and preserve the existing secure-input behavior and messages.
- [Resolved P1 in code; capture pending] Export Backup replaces the Settings experience.
  - Location: Settings → Backup → Export Backup.
  - Evidence: after clicking export backup, the settings sidebar disappears and a legacy form occupies
    the whole content area beneath the global tab bar.
  - Impact: users lose location context and the action behaves like navigation to another application layer.
  - Fix applied: move backup form state into the Settings screen, render it directly in the shared settings
    card, remove the legacy Backup screen route, and keep success/failure feedback inline.
- [Resolved P2 in code; capture pending] Settings controls mix default and shared component styles.
  - Location: Application, Sync, and Terminal settings.
  - Evidence: dropdowns use a different surface/focus treatment from text inputs, while several save and
    folder buttons still use default iced styling.
  - Impact: adjacent controls do not read as one component system.
  - Fix applied: add a shared dropdown style, apply shared primary/secondary buttons throughout settings,
    and add explanatory terminal-renderer copy clarifying that appearance settings remain active.
- [Resolved P2 in code; capture pending] Settings file-chooser buttons use different label metrics.
  - Location: Settings → Sync “Choose folder” compared with Backup / Restore “Choose backup file”.
  - Evidence: both buttons share the same surface and padding values, but Sync wraps its label in an
    explicit 13 px text widget while Backup / Restore uses the standard button-label size, making the
    Sync action visibly shorter and lighter.
  - Impact: adjacent settings pages appear to use two button systems for the same kind of file chooser.
  - Fix applied: construct both buttons from the translated label directly, retain the shared secondary
    style and `[9, 12]` padding, and align both input/button rows with 10 px spacing and centered controls.
    The sidebar label now reads “Backup / Restore” to match the page's expanded responsibility.
- [Resolved P0 in code; end-to-end owner check pending] Backup success discards the generated file.
  - Location: Settings → Backup → Export Backup.
  - Evidence: the UI reports `备份已导出 (960 字节)` without asking for a destination or showing a path;
    code inspection confirmed the daemon returned only the byte count and dropped the encrypted bytes.
  - Impact: no user-recoverable backup file is created despite a success message.
  - Fix applied: return the encrypted bytes over the local protocol, open the native Save dialog before
    export, write only to the user-confirmed path, and report success only after the file write succeeds.
    Cancel produces no export; invalid protocol data and filesystem failures remain inline errors.

## Comparison history

- Iteration 1: source inspected; implementation build and launch passed; capture blocked by the
  inaccessible native window in the current locked/headless display session.
- Iteration 2: owner capture exposed the P1 fill-sizing defect. All four fixed icon badges were
  corrected, and the connection/unlock cards were changed to desktop-oriented horizontal layouts.
  Compilation and Clippy pass; post-fix visual evidence is pending.
- Iteration 3: owner post-fix capture confirms the connection-failure state is desktop-oriented and
  the sizing defect is resolved. Main host and settings states are still required before overall QA.
- Iteration 4: owner normal/focus/error captures exposed the P2 legacy error treatment. The input and
  notice now use the shared Vida tokens and icon library; tests, Clippy, and release build pass.
  Post-fix visual evidence is pending.
- Iteration 5: owner host-editor, quick-connect, and settings captures exposed one P1 layout failure
  and two P2 consistency defects. Code fixes and manual acceptance steps are complete; rebuilt
  post-fix captures are required before the visual gate can pass.
- Iteration 6: owner settings and backup captures exposed a P1 navigation-layer defect and residual
  P2 component inconsistency. Backup now remains inside Settings and all visible settings controls use
  the shared styles. Post-fix visual evidence is pending.
- Iteration 7: owner backup-success capture exposed a P0 functional defect hidden by plausible UI.
  The daemon discarded the ciphertext and the GUI claimed success from a byte count. The full
  choose-destination → transfer bytes → write file → show path flow is now implemented and tested;
  an owner save-dialog/file check is pending.
- Iteration 8: owner Sync and Backup / Restore captures exposed a residual P2 chooser-button height
  mismatch and an ambiguous sidebar label. The two chooser constructors and row metrics are now aligned;
  a rebuilt owner capture is required before the visual gate can pass.

## Implementation checklist

- [x] Central color, radius, surface, button, input, and navigation styles.
- [x] Real icon-library font loaded through iced.
- [x] Scrollable tab strip with pinned right-side actions.
- [x] Host detail, quick-connect panel, settings, unlock, connection, and terminal toolbar restyled.
- [x] Fixed-size badges no longer get overwritten by fill-centering helpers.
- [ ] Capture main and settings states at 1024 × 768.
- [ ] Compare full view and focused regions; fix any P0/P1/P2 differences.

## Follow-up polish

None classified until the implementation screenshot is available.

final result: blocked
