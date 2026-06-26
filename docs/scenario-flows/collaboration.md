# Collaboration scenarios

[Back to scenario index](../scenarios.md)

## Flow 10: publish my work to the team

Business goal: "Share my saved work with everyone else."

Why this flow exists: publishing is a collaborative operation; Draftline must verify the remote did not change before pushing so teammate work is not overwritten.

```mermaid
sequenceDiagram
    participant User
    participant App
    participant Draftline
    participant Remote

    User->>App: Publish
    App->>Draftline: changes
    alt unsaved changes exist
        App-->>User: Save or discard before publishing
    else workspace clean
        App->>Draftline: publish_changes
        Draftline->>Remote: fetch latest remote state
        alt remote has incoming or diverged work
            Draftline-->>App: SyncNeedsMerge
            App-->>User: Review teammate updates first
        else safe to publish
            Draftline->>Remote: push current variation
            Draftline-->>App: PublishResult
        end
    end
```

| Question | Answer |
|---|---|
| Covered today? | Partially covered. |
| Correct primitive path | `changes` -> `preflight_publish` -> `publish`, or legacy `publish_changes` / `publish_changes_with_options`. |
| Safety behavior | `publish_changes` fetches first, checks `sync_status`, and refuses incoming or diverged remote state. Tokenized `publish` also rejects changed local state or changed remote-tracking OID/absence after preflight. Draftline does not force-push. |
| Edge cases | The library enforces a clean workspace before publish even though push itself does not write local files. `NoRemoteVersion` publishes the current variation as a new remote branch. Authentication is supplied by the host through `RemoteOptions`. `SyncNeedsMerge` can wrap either `IncomingAvailable` or `NeedsMerge`; hosts must inspect `SyncStatus.state` before deciding whether to apply incoming work or start merge. A remote branch can still be deleted, recreated, or rewound after the final fetch but before the normal push refspec reaches the server. |
| Gap | Need explicit lease/create-only push mechanics and result types for remote branch disappeared, branch recreated, first-publish race, and remote rewind between final fetch and push. |

## Flow 11: receive teammate updates

Business goal: "Bring in the latest work from the team."

Why this flow exists: applying remote work changes local files and refs, so it is safe only when the workspace is clean and the local variation can fast-forward.

```mermaid
flowchart TD
    A[User selects Get updates] --> B[fetch_remote]
    B --> C[preflight_apply_incoming]
    C --> D{Workspace clean?}
    D -- No --> E[Ask user to save, discard, or shelve]
    D -- Yes --> F{Remote state}
    F -- Incoming available and fast-forward --> G[apply_incoming]
    F -- Up to date --> H[Nothing to apply]
    F -- Local ahead --> I[Nothing incoming]
    F -- No remote version --> J[Nothing incoming]
    F -- Needs merge --> K[Conflict resolution flow needed]
```

| Question | Answer |
|---|---|
| Covered today? | Yes for current-variation fast-forward updates. Diverged clean-merge execution is covered by Flow 12; unresolved conflict UX and broad remote lifecycle diagnostics remain future work. |
| Correct primitive path | `fetch_remote` -> `preflight_apply_incoming` -> `apply_incoming`. |
| Safety behavior | Dirty work blocks apply. Diverged history returns `SyncNeedsMerge` instead of overwriting. Fast-forward ref updates roll back if checkout fails. Apply must also avoid target-tree collisions with untracked, ignored, generated, or policy-excluded files. |
| Edge cases | `preflight_apply_incoming` uses cached remote-tracking state; fetch before preflight for accurate reporting. `apply_incoming` fetches again before applying, so a clean preflight can still fail if the remote changed between preflight and apply. `UpToDate`, `LocalAhead`, and `NoRemoteVersion` return `applied_count: 0`. A remote branch can disappear or be recreated between fetch and apply. |

## Flow 11a: discover teammate-created directions

Business goal: "My teammate published a new option; I want to see and adopt it."

Why this flow exists: collaboration is not only updates to the current variation. Teammates can create new directions that should appear in a product variation picker.

```mermaid
flowchart TD
    A[Teammate publishes new variation] --> B[Local user fetches current variation]
    B --> C{New remote variation fetched?}
    C -- Yes --> D[remote_variations]
    D --> E[adopt_remote_variation]
    C -- No --> F[Need fetch-all or prune diagnostics]
```

| Question | Answer |
|---|---|
| Covered today? | Partially covered. |
| Current support | `remote_variations` lists fetched remote-tracking variations, and `adopt_remote_variation` creates a local variation from one. `fetch_remote` and `sync_status` still operate on the current variation. |
| Safety behavior | Draftline avoids surprising local branch creation; adoption is an explicit call. |
| Gap | Need `fetch_all_variations` or equivalent broad fetch support, plus stale/prune diagnostics for renamed or deleted remote variations. |

## Flow 11b: remote variation was deleted or renamed

Business goal: "A shared option disappeared or changed remotely; explain what happened."

Why this flow exists: stale remote-tracking refs can make deleted or renamed remote work look like it still exists locally.

```mermaid
flowchart TD
    A[Remote variation deleted or renamed] --> B[fetch_remote]
    B --> C{Stale local remote ref remains?}
    C -- Yes --> D[Need prune/stale diagnostic]
    C -- No --> E[Need product message]
```

| Question | Answer |
|---|---|
| Covered today? | Not covered as a business scenario. |
| Current support | Missing remote refs can be ignored during fetch. `remote_variations` can show fetched remote-tracking refs, but current APIs do not yet expose prune/stale lifecycle diagnostics. |
| Safety behavior | Draftline does not delete local variations automatically. |
| Gap | Need prune semantics, stale-ref diagnostics, and user-facing "remote option removed" guidance. |

## Flow 11c: change or remove remote destination

Business goal: "Publish this workspace somewhere else, or stop publishing to this place."

Why this flow exists: changing a remote URL changes where business content is shared and backed up. That deserves explicit confirmation.

```mermaid
flowchart TD
    A[User edits sharing destination] --> B{Remote exists?}
    B -- No --> C[add_remote]
    B -- Yes --> D[add_remote updates URL]
    D --> E[Need confirmation/preflight]
    A --> F[Remove remote]
    F --> G[Not covered today]
```

| Question | Answer |
|---|---|
| Covered today? | Partially covered. |
| Current support | `add_remote` creates a remote or updates the URL of an existing one. `remotes` lists configured endpoints. |
| Safety behavior | Draftline does not push until `publish_changes`, but updating an existing remote URL is currently silent. |
| Gap | Need remote update preflight/confirmation and remove-remote APIs. |

## Flow 12: reconcile teammate changes

Business goal: "My teammate and I both saved changes. Help us reconcile them."

Why this flow exists: divergence is normal collaboration, but resolving it requires user-understandable content conflicts rather than hidden Git merge behavior.

```mermaid
flowchart TD
    A[User tries publish or apply] --> B[fetch_remote]
    B --> C[sync_status]
    C --> D{NeedsMerge?}
    D -- No --> E[Use publish or apply flow]
    D -- Yes --> F[Show incoming and local versions]
    F --> G[Run semantic merge preview]
    G --> H{Conflicts?}
    H -- No --> I[Create merged save]
    H -- Yes --> J[Business-readable conflict UI]
    J --> K[User chooses, edits, or combines]
    K --> I
```

| Question | Answer |
|---|---|
| Covered today? | Partially covered. |
| Current support | `sync_status` detects `NeedsMerge`; `SyncNeedsMerge` prevents unsafe publish/apply; `preflight_merge_incoming` reports the diverged state without mutating; `merge_incoming` writes a clean two-parent merged version through a preflight token; `MergeOutcome`, `MergeConflict`, `ResolverRegistry`, `PlainTextResolver`, and `MarkdownResolver` model conflict results. |
| Safety behavior | Draftline blocks overwrite and routes the user into an explicit merge decision. Clean merge execution re-fetches, validates the tokenized local/remote/base OIDs, checks dirty work and target-tree hazards, uses the operation lock, and writes recovery state before moving files/refs. |
| Gap | Need user-driven execution for unresolved conflicts, richer binary/large-file conflict UX, and host copy for choosing, editing, or combining conflicting content. |
