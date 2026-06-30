export type ScenarioGroup = {
  title: string;
  eyebrow: string;
  summary: string;
  source: string;
  docsPath: string;
  primitives: string[];
  diagram: string;
};

export type Flow = {
  id: string;
  title: string;
  group: string;
  status: string;
  outcome: string;
};

export const scenarioGroups: ScenarioGroup[] = [
  {
    title: "Workspace setup",
    eyebrow: "Start safely",
    summary:
      "Open, initialize, clone, or adopt a workspace without assuming a repository is already shaped for Draftline.",
    source: "docs/scenario-flows/workspace.md",
    docsPath: "/docs/scenarios/workspace/",
    primitives: [
      "init / open / clone_workspace",
      "workspace_summary",
      "preflight_adopt_workspace",
      "inspect",
    ],
    diagram: `flowchart TD
    A[User opens work] --> B{Source}
    B -- New folder --> C[init with policy]
    B -- Existing repo --> D[read-only adoption scan]
    B -- Shared remote --> E[clone workspace]
    C --> F[workspace summary]
    D --> F
    E --> F
    F --> G{Recovery or lock?}
    G -- Yes --> H[show recovery prompt]
    G -- No --> I[normal dashboard]`,
  },
  {
    title: "Content policy",
    eyebrow: "Save the right files",
    summary:
      "Define which workspace files are business content, and surface hazards when policy, Git ignore rules, or previous saves disagree.",
    source: "docs/scenario-flows/content-policy.md",
    docsPath: "/docs/scenarios/content-policy/",
    primitives: [
      "ContentPolicy",
      "audit_content_policy",
      "policy_git_diagnostics",
      "changes",
    ],
    diagram: `flowchart TD
    A[Host defines business content] --> B{Policy rule}
    B -- folders --> C[include paths]
    B -- extensions --> D[include extensions]
    B -- runtime/private --> E[exclude paths]
    C --> F[open workspace with policy]
    D --> F
    E --> F
    F --> G{Git hides or transforms content?}
    G -- Yes --> H[diagnostics and host choice]
    G -- No --> I[safe save surface]`,
  },
  {
    title: "Authoring and versions",
    eyebrow: "Creative iteration",
    summary:
      "Turn edits into named versions, preview old work, branch into variations, switch safely, and restore without erasing history.",
    source: "docs/scenario-flows/authoring.md",
    docsPath: "/docs/scenarios/authoring/",
    primitives: [
      "save_version",
      "create_variation",
      "preflight_switch_variation",
      "restore_version_as_new_save",
    ],
    diagram: `flowchart TD
    A[User edits content] --> B[changes]
    B --> C{Intent}
    C -- keep it --> D[save version]
    C -- try another direction --> E[create variation]
    C -- move directions --> F[preflight switch]
    C -- bring back old work --> G[restore as new save]
    D --> H[history stays understandable]
    E --> H
    F --> H
    G --> H`,
  },
  {
    title: "Collaboration",
    eyebrow: "Team-safe sharing",
    summary:
      "Publish, receive, adopt, and reconcile remote work by fetching first and refusing hidden overwrites.",
    source: "docs/scenario-flows/collaboration.md",
    docsPath: "/docs/scenarios/collaboration/",
    primitives: [
      "preflight_publish / publish",
      "fetch_remote",
      "apply_incoming",
      "merge_incoming",
    ],
    diagram: `sequenceDiagram
    participant User
    participant App
    participant Draftline
    participant Remote
    User->>App: Publish or get updates
    App->>Draftline: fetch and preflight
    Draftline->>Remote: read latest remote state
    alt safe fast-forward or publish
      Draftline->>Remote: push or apply expected ref
      Draftline-->>App: business-shaped result
    else incoming, diverged, or raced
      Draftline-->>App: explicit blocker
      App-->>User: review, merge, or retry
    end`,
  },
  {
    title: "Recovery and cleanup",
    eyebrow: "Simplify without losing recoverability",
    summary:
      "Shelve unfinished work, compact noisy history, clean up old variations, sync hidden support refs, and separate retention from purge.",
    source: "docs/scenario-flows/recovery-cleanup.md",
    docsPath: "/docs/scenarios/recovery-cleanup/",
    primitives: [
      "shelve_changes",
      "apply_history_cleanup",
      "publish_support_refs",
      "preflight_purge_content",
    ],
    diagram: `flowchart TD
    A[User asks to clean up] --> B{Cleanup kind}
    B -- shelve --> C[local shelf ref]
    B -- compact --> D[preview cleanup]
    B -- delete variation --> E[archive old tip]
    D --> F[move visible refs after backup]
    E --> F
    C --> G[recoverable state]
    F --> G
    G --> H{Shared work?}
    H -- Yes --> I[publish hidden support refs]
    H -- No --> J[local recovery point]`,
  },
];

export const flowMatrix: Flow[] = [
  {
    id: "1",
    title: "Start or open a workspace",
    group: "Workspace setup",
    status: "Covered",
    outcome: "Establish a normal workspace, active variation, and recovery-aware dashboard.",
  },
  {
    id: "1a",
    title: "Adopt an existing non-Draftline repo",
    group: "Workspace setup",
    status: "Partially covered",
    outcome: "Inspect first, then let the host choose migration, policy, and remote setup.",
  },
  {
    id: "1b",
    title: "Choose or discover sharing mode",
    group: "Workspace setup",
    status: "Partially covered",
    outcome: "Distinguish local-only work, added remotes, and cloned shared work.",
  },
  {
    id: "1c",
    title: "Add a remote after local work exists",
    group: "Workspace setup",
    status: "Partially covered",
    outcome: "Fetch before first publish and avoid overwriting remote work with the same variation name.",
  },
  {
    id: "1d",
    title: "Start from a shared remote",
    group: "Workspace setup",
    status: "Covered for clone/open and fetched remote variations",
    outcome: "Clone with credentials and discover teammate-published variations explicitly.",
  },
  {
    id: "1e",
    title: "Developer Copilot opens a managed repo",
    group: "Workspace setup",
    status: "Partially covered",
    outcome: "Tell coding agents which Git actions are safe and which Draftline owns.",
  },
  {
    id: "1f",
    title: "Agent uses Draftline APIs directly",
    group: "Workspace setup",
    status: "Partially covered",
    outcome: "Expose inspect, preflight, execute, verify, recovery, and explain surfaces.",
  },
  {
    id: "2",
    title: "Configure what counts as business content",
    group: "Content policy",
    status: "Covered",
    outcome: "Track user content while excluding runtime, generated, or private app state.",
  },
  {
    id: "2a",
    title: "Change content policy after work exists",
    group: "Content policy",
    status: "Not covered",
    outcome: "Future audit and migration should make policy changes honest about old saves.",
  },
  {
    id: "2b",
    title: "Content policy conflicts with Git ignore or attributes",
    group: "Content policy",
    status: "Partially covered",
    outcome: "Warn when policy-tracked content is hidden or transformed by Git behavior.",
  },
  {
    id: "3",
    title: "Understand current state",
    group: "Authoring and versions",
    status: "Covered",
    outcome: "Show active variation, history, dirty files, recovery state, and remote status.",
  },
  {
    id: "4",
    title: "Save business work",
    group: "Authoring and versions",
    status: "Covered",
    outcome: "Create a named, durable version from policy-tracked content.",
  },
  {
    id: "4a",
    title: "Save or shelve selected work",
    group: "Authoring and versions",
    status: "Covered for selected files",
    outcome: "Let users keep ready work while preserving unfinished edits.",
  },
  {
    id: "5",
    title: "Abandon unsaved edits",
    group: "Authoring and versions",
    status: "Covered",
    outcome: "Discard only by explicit, content-policy-aware user choice.",
  },
  {
    id: "6",
    title: "Try another direction",
    group: "Authoring and versions",
    status: "Covered",
    outcome: "Create a stable variation without exposing detached HEAD mechanics.",
  },
  {
    id: "6a",
    title: "Rename or relabel a direction",
    group: "Authoring and versions",
    status: "Covered for display metadata",
    outcome: "Change product labels without rewriting underlying variation identity.",
  },
  {
    id: "7",
    title: "Move between directions",
    group: "Authoring and versions",
    status: "Covered for full-variation switching",
    outcome: "Preflight unsaved work before writing a different variation to disk.",
  },
  {
    id: "8",
    title: "Review older work",
    group: "Authoring and versions",
    status: "Covered",
    outcome: "Preview and diff previous versions without mutating the live workspace.",
  },
  {
    id: "9",
    title: "Restore older work",
    group: "Authoring and versions",
    status: "Partially covered",
    outcome: "Bring an old version back as a new save instead of resetting history.",
  },
  {
    id: "9a",
    title: "Target tree collides with local non-versioned files",
    group: "Authoring and versions",
    status: "Partially covered",
    outcome: "Block checkout-like operations before overwriting ignored or excluded local files.",
  },
  {
    id: "10",
    title: "Publish my work to the team",
    group: "Collaboration",
    status: "Covered for current variation",
    outcome: "Fetch first, verify expectations, and publish without force-overwriting teammates.",
  },
  {
    id: "11",
    title: "Receive teammate updates",
    group: "Collaboration",
    status: "Covered for current-variation fast-forward only",
    outcome: "Apply incoming work only when the local workspace can advance safely.",
  },
  {
    id: "11a",
    title: "Discover teammate-created directions",
    group: "Collaboration",
    status: "Partially covered",
    outcome: "Fetch and explicitly adopt remote-only variations.",
  },
  {
    id: "11b",
    title: "Remote variation was deleted or renamed",
    group: "Collaboration",
    status: "Partially covered",
    outcome: "Prune stale remote-tracking refs and report local-only or remote-only state.",
  },
  {
    id: "11c",
    title: "Change or remove remote destination",
    group: "Collaboration",
    status: "Partially covered",
    outcome: "Treat remote URL changes as explicit sharing-boundary decisions.",
  },
  {
    id: "12",
    title: "Reconcile teammate changes",
    group: "Collaboration",
    status: "Covered for clean semantic merges",
    outcome: "Route divergence into merge preflight and human-readable conflict handling.",
  },
  {
    id: "12a",
    title: "Apply shelved work",
    group: "Recovery and cleanup",
    status: "Covered for clean all-work shelves",
    outcome: "Preview, preflight, and apply shelves without deleting the recovery point automatically.",
  },
  {
    id: "13",
    title: "Remove or clean up work",
    group: "Recovery and cleanup",
    status: "Covered for local delete, local squash, and local milestone compaction",
    outcome: "Archive old tips under Draftline support refs before visible refs move.",
  },
  {
    id: "13a",
    title: "Compact local version history",
    group: "Recovery and cleanup",
    status: "Covered for linear milestone compaction",
    outcome: "Simplify noisy saves while preserving final content, backups, and stale-version mapping.",
  },
  {
    id: "13b",
    title: "Remove or rewrite shared work",
    group: "Recovery and cleanup",
    status: "Covered for shared variation delete, current-variation replacement, and published compaction",
    outcome: "Publish support refs first, then change visible shared refs with leases.",
  },
  {
    id: "13c",
    title: "Sync hidden recovery support refs",
    group: "Recovery and cleanup",
    status: "Covered for local publish/fetch/restore",
    outcome: "Move hidden recovery refs across machines without showing them as normal variations.",
  },
  {
    id: "13d",
    title: "Recover cleanup after clone or device loss",
    group: "Recovery and cleanup",
    status: "Covered for fetched support refs",
    outcome: "Fetch support refs and restore archived work as a visible variation.",
  },
  {
    id: "13e",
    title: "Sync incoming compacted remote history",
    group: "Recovery and cleanup",
    status: "Covered for safe incoming rewrites",
    outcome: "Recognize published compaction and replay safe local-only saves.",
  },
  {
    id: "13f",
    title: "Permanently purge or redact content",
    group: "Recovery and cleanup",
    status: "Planning-only",
    outcome: "Separate destructive purge from normal archive-first cleanup.",
  },
  {
    id: "13g",
    title: "Expire old support refs",
    group: "Recovery and cleanup",
    status: "Partially covered locally",
    outcome: "Clean retention refs without promising sensitive-data deletion.",
  },
  {
    id: "13h",
    title: "Large or binary business assets",
    group: "Recovery and cleanup",
    status: "Partially covered",
    outcome: "Detect binary or large files so hosts can warn, block, or externalize.",
  },
  {
    id: "14",
    title: "Recover from interruption or unusual state",
    group: "Recovery and cleanup",
    status: "Partially covered",
    outcome: "Block normal flows until interrupted operations are diagnosed and repaired.",
  },
  {
    id: "14a",
    title: "Out-of-band Git mutation",
    group: "Recovery and cleanup",
    status: "Partially covered",
    outcome: "Detect direct Git changes that bypass Draftline's safety model.",
  },
  {
    id: "14b",
    title: "Stale or abandoned operation lock",
    group: "Recovery and cleanup",
    status: "Partially covered",
    outcome: "Clear stale metadata locks only when the workspace proves it is safe.",
  },
];

export const safetyPrinciples = [
  "Users choose product actions, not Git commands.",
  "Look-around flows are read-only.",
  "Moving, restoring, publishing, and cleanup flows preflight first.",
  "Shared operations fetch and compare explicit remote identity before mutating.",
  "Cleanup archives old tips unless the user chooses a separate purge/redaction workflow.",
  "Draftline IDs and variation metadata should round-trip without parsing Git branch names.",
];

export const statusTone = (status: string) => {
  if (status.startsWith("Covered")) {
    return "covered";
  }

  if (status === "Planning-only" || status === "Not covered") {
    return "planned";
  }

  return "partial";
};
