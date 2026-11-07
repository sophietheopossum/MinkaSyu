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

Possible future enhancement (not v1): surfacing "shadowed" updates
where a later repo in pacman.conf order has a newer version than the
one pacman would pick. A shell-script prototype exploring that is in
this repo's history (the initial commit).
