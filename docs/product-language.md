# Draftline product language

[Back to scenario index](scenarios.md)

## Product language mapping

| Product action | Git-backed implementation | User-facing framing |
|---|---|---|
| Save | Commit | Saved version |
| Set up existing repo | Read-only diagnostics plus explicit setup choices | Adopt workspace |
| Try another direction | Branch | Variation |
| Show older content | Tree preview | Preview version |
| Bring back older content | New commit from old tree | Restore as new save |
| Share | Push current variation | Publish changes |
| Get teammate updates | Fetch plus fast-forward | Apply incoming changes |
| Reconcile teammate changes | Three-way merge and conflict resolution | Resolve changes |
| Abandon edits | Policy-aware checkout/reset/removal | Discard changes |
| Put work aside | Local shelf support ref by default | Shelve changes |
| Remove option | Delete branch after archive | Delete variation |
| Remove shared option | Archive support ref, then expected-OID remote deletion | Remove variation for the team |
| Clean up local history | Rewrite branch after archive | Squash versions |
| Replace shared history | Archive support ref, then consented force-with-lease replacement | Replace shared history |
| Recover hidden support state | Fetch/list/restore `refs/draftline/...` | Recover archived work |
| Permanently delete sensitive content | Not yet exposed | Purge or redact content |
