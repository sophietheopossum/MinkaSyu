# MinkaSyu

Sophie's update utility for Arch/CachyOS: a Rust core plus a window
on AntiWebUI (`gui/`, the `minka-syu-gui` binary). Checking is MinkaSyu's job (scheduled, safe — sync DBs are
refreshed into a throwaway copy, so a check can never cause a partial
upgrade); applying is paru's job, launched interactively in a terminal.
Repo and AUR updates land in one list in one pass.

Every update carries repository provenance — where the installed
package came from and where its candidate will come from — and a
change of repository is displayed explicitly rather than silently.

Not a pamac replacement: it browses, installs and removes through paru
in a terminal, with no transaction logic of its own.

How paru ended is not lost with its window. The terminal runs
`minka-syu run …`, which runs paru under script(1) (everything it
shows also goes to a log in the state dir's `runs/`), writes how it
ended to a result file, and on a failure keeps the window open until
Enter. The window shows a failure as a card above the list: the
command, its exit status or signal (and which packages did not build),
pacman's and paru's `error:` lines (with what pacman lists under them),
each makepkg `==> ERROR:` with the build lines before it, what came
after, and the log's path. A step that exits 0 but printed error lines
that still matter (a failed pacman hook or scriptlet, dkms, mkinitcpio)
gets an amber card and keeps its terminal open as well; a mirror that
pacman gave up on for the next one does not count. A no at a question,
Ctrl+C, or closing the window at a question is a cancel: no card. Each
step has its own card until it is dismissed. From the CLI,
`minka-syu apply|install|remove` wait for the terminal and print a
failure to stderr with exit status 1. Only the newest 8 steps' files
are kept, and a clean step's log is deleted.

## The window

`minka-syu ui` opens it (the .desktop entry and MinkaShell's updates
pill do). It runs `minka-syu-gui` (beside the core binary, or
`$MINKA_SYU_GUI`), a view over `minka-syu serve` on its stdio; only one
window runs per session, and a second launch raises it. The QML window
in `ui/` is kept as the fallback: `MINKA_SYU_FRONTEND=quickshell`, or
whenever `minka-syu-gui` is missing (`$MINKA_SYU_DIR` points `qs -p` at
a checkout's `ui/`). `minka-syu-gui --tab updates|browse|installed|sources`
opens on a tab, and `--detail <pkg>` on a package's detail view.

The QML fallback does not know switch rows or the no-recorded-repository
row. It shows them as later-repo rows, and while they are unticked its
command preview lists them under `--ignore` (the group as
`(no recorded repository)`). The core drops those names before paru
runs: an unticked switch or reinstall is only left out. Its detail view
does not keep an installed build either: choosing that build, or the
first repository's copy, for a later-repo row leaves the row ticked with
nothing to do.

When the AUR has a package, its detail view links to the package's AUR
page (comments, votes, maintainer) under the repository builds.

The detail view of an installed package shows its install reason as
two buttons, Dependency and Explicit. A press opens the terminal on
`sudo pacman -D --asdeps|--asexplicit -- <pkg>` (the "reason" step,
reported like the others); the buttons show what the local DB says,
read again when that step ends, and never flip before. A failed change
says so under the buttons too, as long as its card is there. From the
CLI: `minka-syu reason [--json] <pkg>` shows it, `minka-syu reason
[--dry-run] --asdeps|--asexplicit <pkg>` changes it.

Who actually needs a package is a narrower question than "Required
By", and `reason` answers it too: the installed packages that `pacman
-Rc` would remove along with it. A package needs it when one of its
dependencies is satisfied only by it, or by a package that already
needs it, so a dependency another installed package satisfies as well
(a second provider of java-runtime or sh) does not count, and neither
does an optional one; versions and provides match as for Required By.
The explicitly installed packages among them are what keeps it wanted.
With none, nothing installed on purpose needs it, which `pacman -Qdt`
misses when dependencies require each other in a cycle. What excuses a
package that requires it without needing it is named too: the installed
packages that would stay and satisfy that dependency as well (for
ca-certificates, ca-certificates-utils, which provides it; a provider
that itself goes with the package is none). `reason` says how many
explicitly installed packages need it, or that none do, and what covers
the rest; `--json` lists those as `needed_by_explicit`, every package
that needs it as `all_required_by`, and the providers as `covered_by`.

In the window, NEEDED BY under the install reason says it in one line:
how many explicitly installed packages need it, and the first few; or,
in amber, that none do and only dependencies need it (named), or that
nothing needs it because other installed packages (named) also provide
what requires it. When some need it and other packages that require it
are covered, "· others are covered by …" names the providers, so an
explicitly installed package under "Required by" that is missing here
has its reason. A package nothing requires gets the orphan line instead:
as a dependency it is an orphan, which `pacman -Qdt` lists; if something
has it as an optional dependency, only `pacman -Qdtt` does. A package
that depends on itself (perl-business-isbn-data) is in its own "Required
By", so neither lists it; when nothing else requires it, the line says
so, and it is left out of its own lists. What it is optional for comes
after. "Required by" and "Optional for" (pacman's "Required By" and
"Optional For", versions and sonames matched as libalpm matches them)
and "Explicit packages that need it" open lists of names, the first 12
until "Show all", with explicitly installed packages marked. A name
opens that package's detail view, and "Back to <package>" in its header
returns; closing the view forgets the way back.

"Remove…" in the footer removes the package in the terminal with every
package that needs it, which pacman would not leave broken: `paru -Rns
-- <pkg> <what needs it>` (the "remove" step, as from the Installed
tab), and paru asks before anything goes. All of those are dependencies,
since the button is off while anything explicitly installed needs the
package. The command shows under NEEDED BY before the press, which
reads the local DB again and opens the terminal only if the targets are
still those. The button is off while an install reason change runs
(it can make one of them needed), and while its own terminal is open,
when Dependency and Explicit do nothing either and the line says what
it waits for; when the step ends, the view and the Installed tab's list
(once it has been read) read the local DB again, and a failed removal
says so under NEEDED BY as long as its card is there (the next removal
of the same packages replaces it, as the next change of a package's
install reason replaces that one's). The QML fallback
has neither the section nor the button.

Tabs: **Updates** (below), **Browse** (search repos + AUR, tick builds
to install together), **Installed** (filter, tick to remove), and
**Sources**, shown only when a `minkarepo` is found
(`$MINKA_SYU_MINKAREPO`, PATH, `~/.local/bin`): the MinkaPKG packages
against their upstreams (`minkarepo check --tsv`, cached in the state
dir as `sources.json`). Tick what to bump and "Update & publish…" opens
the terminal on `minkarepo update <picked>`; packages marked
`auto=yes` with something to do start ticked. "don't track" writes
`track=no` into the package's `upstream` file (`minkarepo track`), and
the check stops asking about it.

What the list holds beyond `pacman -Qu`, which matches packages by
name only. Where it can, the check asks libalpm itself: `pacman -Sup
-dd` on the private DB prints `-Su`'s own target set (print mode needs
no root or lock), which settles what a plain `-Syu` installs and from
where. If that fails, the check falls back to `-Qu` and pacman.conf.

- **Later-repo updates.** A strictly newer version in a repo ranked
  below the one pacman would pick ("later repo"), for a package `-Syu`
  leaves alone, since its first repository's copy is not newer than the
  installed one. These start unticked and are applied as explicit
  `repo/name` targets, because a plain `-Syu` never selects them. If
  that build replaces an installed package, the row says so. In the
  window's detail view, any build chosen for such a row is a target too,
  except the installed one: "Keep installed build" unticks the row and
  keeps no choice (choosing it used to leave the row ticked with nothing
  to do, bpf on 23/11/2026). The repository the package was installed
  from, at the installed version, counts as the installed build even
  when it rebuilt that version in place (the Installed tab reinstalls
  such a copy). On an ordinary upgrade the same button unticks the row:
  `--ignore`, for that run. A chosen build that is installed by the next
  check (the run applied it) goes the same way, its row unticked: the
  row's own build, passed over for it, never comes ticked after it.
- **Newer builds of an upgrade.** When `-Syu` upgrades a package and a
  repo ranked below has a strictly newer build still, the row stays
  pacman's upgrade, ticked, and notes the other build ("newer build in
  extra-x86-64-v4: 620.73.04-1.1"); the detail view offers it, and
  choosing it makes the row a `repo/name` target. Offering that build as
  the row, unticked, held back pacman's own upgrade as well, since an
  unticked row is `--ignore` (nvidia-settings, libxnvctrl and sqlite on
  23/11/2026, where ALHP's builds were newer than CachyOS's only by their
  rebuild suffix). If that build replaces an installed package, the row
  says so beside the note, and once the build is chosen it is the row's
  replacement, as on a later-repo row. The
  row's repository change is pacman's own, from where the package was
  installed to pacman's candidate; choosing the noted build makes it a
  change to that build's repository instead, which may add one or take
  one away (a package installed from the lower repository changes back
  to the first one unless its build is chosen). The QML fallback shows
  such a row as a plain upgrade, without the note or what the noted
  build replaces.
- **Same-version switches.** An installed package whose build is in no
  repository, while a repository carries that version as a build of its
  own: its repository dropped it (Arch's `[multilib]` lib32-libcaca,
  rebuilt in `[minka]` at the same 0.99.beta20-2), or it came from the
  AUR or a file. Nothing is newer, so `-Qu` and `-Syu` never see it.
  Builds are told apart by build date and packager: the local DB's
  against each repository copy's, from one `pacman -Sp` per round. The
  row offers the first repository in pacman.conf order with that
  version, and reads like a repo change with the same version on both
  sides ("switch to minka's build"). Like a later-repo row it starts
  unticked and is applied as a `repo/name` target, which pacman
  reinstalls from there. Unticked, it is only left out, never passed to
  `--ignore`. A package with no recorded repository whose build a
  repository has is that repository's package, not a switch.
- **No recorded repository.** libalpm records where a package came from
  (`%INSTALLED_DB%`) only for a named target or a plain upgrade, so a
  package pacman pulls in as a dependency or a replacement has none until
  its next upgrade (78 packages on 22/11/2026, 77 of them dependencies).
  Those whose build a repository carries (same version, build date and
  packager, from the switches' lookup) form one row, counted as one
  update everywhere (the window, notifications, MinkaShell's pill), with
  its names shown on demand and tickable one by one. Ticked, each is
  reinstalled from that repository (the first in pacman.conf order whose
  copy is the build) as a `repo/name` target of the same `paru -Syu`,
  which records it. Like a switch it starts unticked and is never
  passed to `--ignore`; the CLI's `apply` includes it, less any member
  named in `--ignore` (`--ignore '(no recorded repository)'` leaves out
  all of them). IgnorePkg/IgnoreGroup keep a package out of it, and so
  does a conflict: a package that something the run may install
  conflicts with (an upgrade in `-Su`'s set, or a later-repo or switch
  row's build, ticked or not) would make pacman abort the whole
  transaction as a `repo/name` target, where without one it asks to
  remove it. A replacement keeps its package out too. A switch row is
  dropped for the same conflict. Still unseen: conflicts of dependencies
  new to the run, and the package's own `%CONFLICTS%`. In the window the
  group's box shows a mix when some names are left out; leaving a name
  out only drops its reinstall, and never carries over as `--ignore`
  when that package later gets an update of its own.
- **Install reasons survive reinstalls.** A sync reinstall keeps the
  installed package's `%REASON%` (libalpm's commit_single_pkg copies it
  from the old package), with or without `-y -u`, and paru passes
  `repo/name` targets on unchanged; checked in a scratch root on
  22/11/2026 with pacman 7.1.0 (CachyOS) and paru 2.2.0. So a switch or
  a reinstall from the row above leaves dependencies dependencies, and
  no `pacman -D` stage follows. Only `--asdeps`/`--asexplicit` (every
  target's reason) or `--needed` (skips same-version targets) would
  break that, and MinkaSyu passes neither.
- **Replacements.** A package whose `replaces` names an installed
  package, such as `minka/coreutils-uutils` replacing the orphaned
  `coreutils-hybrid`. `-Qu` never reports these. Which installed
  packages each target removes follows libalpm's own walk
  (alpm_sync_sysupgrade: installed packages in name order, the repos
  they upgrade from in order, replacers before the same name). They
  start ticked, like pacman's `Replace …? [Y/n]`, and plain `-Syu`
  applies them. Unticking one passes it to `--ignore`, which skips the
  replacement. Only the replacing repo's build can be chosen for it: a
  same-named package from another repo has no `replaces` and would
  only conflict.
- **Replaced packages.** A package that a replacement removes keeps
  its own row, marked "replaced by …", for information. Unticking the
  replacement skips it; what pacman then does to the package (upgrade,
  hold, another replacer) depends on the whole run, so the row does not
  claim an outcome, and the next check shows it. A package the same run
  installs again (an Arch split whose new part depends on it) is listed
  as an ordinary upgrade instead. A replacer that is itself replaced
  stays ticked until the outer replacement is unticked, because
  ignoring it would cancel that one too.
- **Held packages.** Packages `-Syu` skips because of pacman.conf are
  listed dimmed with a "held" badge, so a pending version is not
  forgotten: IgnorePkg/IgnoreGroup (on the installed package or the
  new build), or a newer build only in a repo without
  `Usage = Upgrade`. They cannot be ticked or given a repo, and they do
  not count towards pending updates, notifications or MinkaShell's bar
  pill.

**Untracked packages** are an error, not an update: an installed package
nothing tracks. The check lists them after the updates, and the window
in a red card above the list (dismissed until the list changes); each
with its version, install reason, what requires it, and what to do.
Two kinds:

- in no repository, and the AUR has no package of that name (one AUR RPC
  info request per check, POSTed through curl, for every candidate;
  `$MINKA_SYU_AUR_RPC` overrides the URL). paru checks such "foreign"
  packages against the AUR, so an AUR package of that name is tracked;
- a repository has the name but not this version, no repository was
  recorded, and libalpm validated it with nothing (`%VALIDATION%`
  none): installed with `-U` from a file with no signature. paru never
  asks the AUR about a name a repository has (it splits packages by
  that), so even an AUR build of it is untracked (python-async-timeout
  5.0.1-2 on 22/11/2026, the AUR's, while `[extra]` has 5.0.1-1). A
  missing `%INSTALLED_DB%` alone means nothing (dependencies never get
  one), and a signed or checksummed build is a repository's, which
  pacman tracks by name: a v4 repository's 1.68-1.1 that it dropped,
  while `[extra]` has 1.68-1, is no error.

When the AUR cannot be asked, no package of the first kind is flagged
(most are AUR packages) and an amber note says so instead. A package
whose recorded repository moved past it is tracked by that repository,
and a row (an update, a replacement) keeps a package off the list.

## Comparing sources

`minka-syu compare <pkg> <repo-a> <repo-b>` opens meld on the source
trees (the PKGBUILD and the files beside it) behind two repositories'
builds of a package, `aur` standing for the AUR's current version:
`minka-syu compare nvidia-settings cachyos-v4 extra-x86-64-v4`. With
`--print` it only prints where the two trees are and where they came
from. `serve` has it as `compare` {package, left, right}, on a thread of
its own; it answers {package, left, right}, each side's repo, version,
dir, how sure the tree is (`matched`) and its provenance lines, and
opens meld. meld is an optional dependency: without it the trees are
still written, and the error says so.

In the window, when a package's detail view offers two builds or more,
each (the AUR's too) has a Compare chip at the end of its row, apart
from its box: a press picks the build for meld without selecting it,
and the chip then reads Left or Right, meld's side for it, in the order
the two were picked. A press on a picked build unpicks it, and a third
pick replaces the older one. Nothing is picked when the view opens, and
the picks go when it shows another package (a dependent, Back) or
closes. "Compare in meld" beside the action button is on with two
builds picked, and the footer says what it opens ("Compare cachyos-v4
with extra-x86-64-v4 in meld"), or with one, to pick one more, unless
it says why the action button is off. The button is off while a compare
runs; meanwhile the footer says "Fetching sources for meld… (<left>
against <right>)", and a failure shows in the error bar above it.
Compares run one at a time across processes too (`compare/.lock`), so
the window and the command line never write one tree at once. The QML
fallback has no such button.

Who built a package decides where its tree is, told by repository and
packager, never by architecture or a ".N" suffix (CachyOS's v4
repositories have x86_64 files with ".1", like ALHP's):

- Arch (`core`, `extra`, `multilib`): the package base's GitLab project
  at the tag of its version, exact by construction (Arch's repositories
  take a package only with the PKGBUILD at its signed tag).
- ALHP and CachyOS's rebuilds of Arch (`cachyos-core-v4`,
  `cachyos-extra-v4`): that tag for the version less its ".N", with the
  pkgrel line rewritten to the suffixed one, as they build it (ALHP's
  builds since 21/9/2026 also turn a `name<=V-R` in conflicts or replaces
  into `name<V-(R+1)`; the .BUILDINFO hash says which form a build used).
  The rest of the difference is build flags: ALHP publishes its
  makepkg.conf (alhp.dev/makepkg), CachyOS does not.
- CachyOS's own packages (`cachyos`, `cachyos-v4`, ...): CachyOS-PKGBUILDS,
  linux-cachyos or cachyos-aur-derived on GitHub, at the commit whose
  PKGBUILD has the sha256 the build's .BUILDINFO names (also as CachyOS's
  builder edits it: its `_optimize_v4` switch on, or without the final
  newline), else the newest commit naming the version by the build date
  (plus 2 days, as CachyOS's commits can trail its builds), else the
  newest commit before the build, marked approximate. Every repository
  is searched (a pkgbase can be in two), and for `cachyos` the AUR's
  history too, as it is or with the pkgrel rewritten: it built AUR
  PKGBUILDs before cachyos-aur-derived began. A hash match whose local
  files do not match the PKGBUILD's checksums says so ("PKGBUILD exact;
  local files … differ": gcc's patches were never published). Kernels
  are always approximate: their builder edits each variant's PKGBUILD.
- The AUR: the package base's git repository, at the commit a built
  package in the pacman cache or paru's clone directory names, else the
  newest commit naming the version (by the installed build's date when
  it is paru's build).
- `[minka]`: MinkaPKG, the checkout minkarepo lives in, only read: the
  commit the build's .BUILDINFO names, else the uncommitted working tree
  when its PKGBUILD is that one, else a commit naming the version,
  marked unverified.
- Any other repository (kotontrion): "no known source".

A repository build's .BUILDINFO comes from `/var/cache/pacman/pkg`, or
else the first 256 KiB of the package from its repository's servers
(`pacman-conf -r`); it also marks Arch, ALHP and rebuild trees as
hash-verified or not. Each tree is written afresh to
`$XDG_CACHE_HOME/minka-syu/compare/<repo>/<pkgbase>/<version>/` with a
`MINKA-SYU-SOURCE.txt` on top (repository, package, pkgbase, version,
packager, build date, source and revision, how it was matched, build
flags), the same name on both sides so meld lines them up. The GitHub
and AUR clones are kept under `compare/git/` (CachyOS's blobless) and
fetched again after 10 minutes. What is downloaded is only data: no
PKGBUILD is sourced or built, archives are unpacked with bsdtar's
defaults (no absolute paths, no `..`, nothing written through a
symlink), a symlink in a tree becomes a file saying where it pointed
(meld would follow it), and requests carry a generic
`minka-syu/<version>` user agent, with git run without the user's
configuration and unable to prompt for credentials.
