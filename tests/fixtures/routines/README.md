# Routine fixtures

Copies of Harness Desktop's routine file fixtures (`business/tests/fixtures/routines/` in the desktop repository, format in `docs/routines-format.md` there), used to check the team routine body rules. Keep `valid-*.json` and `invalid-*.json` identical to the desktop copies, the desktop's `approvals/` folder and `.gitattributes` are not copied. Two files exist only here.

A fixture is checked by content only: its name does not follow `<id>.json`, and the `id` inside it is the one to use.

A team body is a version 1 routine file with these rules on top (the format doc, "Team routines"):

- `seed` and `requires` are never set. Routine files have no Harness family catalogue, so a body with either is simply invalid, in every copy of the format.
- A project routine's `project` is the repository key `git:host/owner/repo`, never a local path. A global routine's `project` is `null`.
- A body that holds a credential is refused, like a hook body.

What that means for these fixtures:

| File | Team body check |
| --- | --- |
| every `invalid-*.json` except the two below | invalid |
| `invalid-project-relative.json` | a valid body shape whose `project` is not a repository key: refused, like the hook fixture of the same name |
| `invalid-team-local-path.json` (only here) | the same file as a valid project routine, but its `project` is the absolute path `/home/user/work/app`: refused as a team body, the desktop accepts it as a local routine |
| `valid-project-scope-agent.json`, `valid-project-script-repo.json` | valid desktop files whose `project` is a local path: refused as team bodies |
| `valid-team-project.json` (only here) | project routine with `project` `git:github.com/acme/app`: accepted, and shared only for the repository `git:github.com/acme/app` |
| every other `valid-*.json` | accepted (global routines) |

The two `refused` rows are why a loader cannot treat every `invalid-*` file as invalid: handle `invalid-project-relative.json` and `invalid-team-local-path.json` as hooks' tests handle `invalid-project-relative.json`.

`valid-team-project.json` is not a valid local routine file (the desktop requires an absolute path there). The desktop reads team bodies with the repository-key form of `project`, and may reuse this file for that.
