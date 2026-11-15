# MinkaSyu

Sophie's update utility for Arch/CachyOS: a Rust core plus Quickshell
front end. Checking is MinkaSyu's job (scheduled, safe — sync DBs are
refreshed into a throwaway copy, so a check can never cause a partial
upgrade); applying is paru's job, launched interactively in a terminal.
Repo and AUR updates land in one list in one pass.

Every update carries repository provenance — where the installed
package came from and where its candidate will come from — and a
change of repository is displayed explicitly rather than silently.

Not a pamac replacement: no search/browse/AppStream, no transaction
logic of its own.

What the list holds beyond `pacman -Qu`, which matches packages by
name only:

- **Later-repo updates.** A strictly newer version in a repo ranked
  below the one pacman would pick ("later repo"). These start unticked
  and are applied as explicit `repo/name` targets, because a plain
  `-Syu` never selects them.
- **Replacements.** A package whose `replaces` names an installed
  package, such as `minka/coreutils-uutils` replacing the orphaned
  `coreutils-hybrid`. `-Qu` never reports these. The check asks
  libalpm for its own sysupgrade target set (`pacman -Sup -dd` on the
  private DB) and keeps the entries that, by libalpm's rules, really
  remove an installed package. They start ticked, like pacman's
  `Replace …? [Y/n]`, and plain `-Syu` applies them. Unticking one
  passes it to `--ignore`, which skips the replacement. Only the
  replacing repo's build can be chosen for it: a same-named package
  from another repo has no `replaces` and would only conflict.
- **Held packages.** Packages held back by IgnorePkg/IgnoreGroup are
  left out. `-Qu` lists them with `[ignored]`, but `-Syu` will not touch
  them.
