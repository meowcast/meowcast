#!/bin/sh
# End-to-end test of repositories and the Store app's requests: the real appd (host build)
# runs in the builder container against three local stores made with the publishing tools,
# and a screenless build of the Store app (`store selftest`) talks to it from its sandbox.
#
# Run from apps (privileged: appd builds cgroups and sandboxes; the default network
# gives it the default route it waits for, nothing leaves the container):
#   docker run --rm --privileged -v "$PWD/..:/fw:ro" -v meowcast-build:/build \
#     -w /fw/apps meowcast-builder:gpu sh test-store.sh
set -eu
export LC_ALL=C CARGO_TARGET_DIR=/build/apps-target
host=$(rustc -vV | sed -n 's/^host: //p')
cargo build --release --locked --offline --config .cargo/vendor.toml -p appd -p store --target "$host" 2>&1 | tail -1
bin=$CARGO_TARGET_DIR/$host/release
appd=$bin/appd
w=/tmp/store-test
rm -rf $w /data/appd /data/appdata
mkdir -p $w/payload/bin $w/keys /data
cp "$bin/store" $w/payload/bin/app
fails=0
check() { # check "what" command...: the command must succeed
    what=$1; shift
    if "$@" >/dev/null 2>&1; then echo "ok    $what"; else echo "FAIL  $what"; fails=$((fails + 1)); fi
}
has() { grep -q -- "$2" "$1"; }
lacks() { ! grep -q -- "$2" "$1"; }
manifest() { # manifest ID NAME PERMISSIONS [EXTRA]
    printf '{"id":"%s","name":"%s","version":"1.0","version_code":1,"exec":"bin/app","publisher":"test","permissions":[%s]%s}' "$1" "$2" "$3" "${4:-}" > $w/app.json
}
pack() { # pack STORE KEY
    "$appd" pack --manifest $w/app.json --payload $w/payload --key "$2" --store "$1" >/dev/null
}

# The built-in store (key A): the Store app without a screen, and one ordinary app.
"$appd" keygen $w/keys/a.key >/dev/null
"$appd" keygen $w/b.key >/dev/null
"$appd" keygen $w/c.key >/dev/null
requests='"selftest","probe|http://127.0.0.1:8003/","add|http://127.0.0.1:8003/|@key","refresh","install|net.example.third|allow","install|net.example.needy|deny","uninstall|net.example.third","uninstall|dev.meow.storetest","open|org.example.hello","enable|rnone|0","add|http://127.0.0.1:8003/|0000 0000 0000 0000","probe|http://127.0.0.1:8002/","probe|http://8.8.8.8/"'
manifest dev.meow.storetest "Store test" '{"name":"manage-apps"}' ",\"args\":[$requests]"
pack $w/a $w/keys/a.key
manifest dev.meow.plain "Plain" '{"name":"storage","quota_kb":16}'
pack $w/a $w/keys/a.key
"$appd" repo --store $w/a --key $w/keys/a.key --name "meow store" >/dev/null
# A second repository (key B): an app of its own, and three things it must not get away
# with: an id of the built-in store, a system service, an app with "manage-apps".
manifest org.example.hello "Hello" '{"name":"storage","quota_kb":16},{"name":"internet","hosts":["example.org"]}'
pack $w/b $w/b.key
manifest dev.meow.plain "Plain (fake)" ''
pack $w/b $w/b.key
manifest org.example.service "Service" '{"name":"network"}' ',"service":"net"'
pack $w/b $w/b.key
manifest org.example.manager "Manager" '{"name":"manage-apps"}'
pack $w/b $w/b.key
"$appd" repo --store $w/b --key $w/b.key --name "Example apps" >/dev/null
# A third (key C), which the Store app adds: one app, and one that needs a sensitive permission.
manifest net.example.third "Third" ''
pack $w/c $w/c.key
manifest net.example.needy "Needy" '{"name":"network"}'
pack $w/c $w/c.key
"$appd" repo --store $w/c --key $w/c.key --name "Third party" >/dev/null
# appd hands out cgroup controllers, which the kernel allows only from a group without
# processes: move everything here out of the container's root group first.
mkdir -p /sys/fs/cgroup/test-shell
for p in $(cat /sys/fs/cgroup/cgroup.procs); do echo "$p" > /sys/fs/cgroup/test-shell/cgroup.procs 2>/dev/null || true; done
servers=
for s in a:8001 b:8002 c:8003; do
    python3 -m http.server "${s#*:}" --bind 127.0.0.1 --directory "$w/${s%%:*}" >/dev/null 2>&1 &
    servers="$servers $!"
done
sleep 1 # the servers are listening
export APPD_KEYS=$w/keys APPD_STORE=http://127.0.0.1:8001/
start() { "$appd" daemon >>$w/appd.log 2>&1 & daemon=$!; sleep 3; }
stop() { kill "$daemon" 2>/dev/null || true; sleep 1; }
ctl() { "$appd" ctl "$@" >$w/out 2>&1 || true; }
start
ctl list
check "the built-in store is listed" has $w/out "dev.meow.plain"
check "the Store app was installed by the system" has $w/out "dev.meow.storetest *installed"
check "an ordinary app waits for the user" has $w/out "dev.meow.plain *available"
ctl uninstall dev.meow.storetest
check "the Store app cannot be uninstalled" has $w/out "part of the system"
ctl repo
check "one repository to begin with" test "$(grep -c . $w/out)" = 1

ctl repo add http://8.8.8.8/
check "a public address must be https" has $w/out "https://"
ctl repo add http://127.0.0.1:8001/
check "the built-in store is not added twice" has $w/out "already on the list"
ctl repo add 127.0.0.1:8009
check "an address with nothing behind it is refused" has $w/out "appctl:"
ctl repo add http://127.0.0.1:8002/index.json
check "a repository is added by its address" has $w/out "added Example apps"
b=$(sed -n 's/.*as \(r[0-9a-f]*\),.*/\1/p' $w/out)
sleep 2
ctl repo
check "it is listed with its key and its apps" has $w/out "$b  Example apps  http://127.0.0.1:8002/  .*  1  0  ok    1"
ctl list
check "its app is offered" has $w/out "org.example.hello *available"
check "it cannot take an id of the built-in store" lacks $w/out "fake"
check "it cannot ship a system service" lacks $w/out "org.example.service"
check "it cannot ship an app with manage-apps" lacks $w/out "org.example.manager"
check "the refusals are logged" test "$(grep -c "rejected\|already provided" $w/appd.log)" -ge 3
ctl install org.example.hello all
check "its app installs" has /data/appd/installed/org.example.hello/source.json "\"repo\": \"$b\""
check "an app of the built-in store records no repository" lacks /data/appd/installed/dev.meow.storetest/source.json "repo"

# An update signed with another key (the host was taken over) is not taken.
manifest org.example.hello "Hello (taken over)" '{"name":"storage","quota_kb":16},{"name":"internet","hosts":["example.org"]}'
pack $w/b $w/c.key
ctl refresh; sleep 2
ctl info org.example.hello
check "an update signed with another key is refused" has $w/out "version_code 1,"
check "and the refusal is logged" has $w/appd.log "org.example.hello: signature does not match"
pack $w/b $w/b.key
rm -rf $w/b/apps/org.example.hello/2 && "$appd" index --store $w/b >/dev/null

# The pinned key survives a restart; a record that names a repository that is gone does not.
stop; start
ctl list
check "after a restart the repository's app is still installed" has $w/out "org.example.hello *installed"
ctl repo off "$b"; ctl list
check "switched off: installed apps stay" has $w/out "org.example.hello *installed"
ctl repo
check "switched off: listed as off" has $w/out "$b .*  0  0  off"
ctl repo on "$b"; sleep 2
ctl repo
check "switched on again" has $w/out "$b .*  1  0  ok"

# The Store app's own requests, from inside its sandbox.
ctl launch dev.meow.storetest
sleep 14
ctl logs dev.meow.storetest; cp $w/out $w/store.log
check "the Store app got the lists" has $w/store.log "STORE end"
check "it sees the repositories" has $w/store.log "STORE repo | $b | Example apps"
check "it sees apps of both repositories with their permissions" has $w/store.log "STORE app | org.example.hello | $b | Hello .*internet|sensitive|1|granted|Connect to example.org"
check "it sees itself as part of the system" has $w/store.log "STORE app | dev.meow.storetest | meow | Store test .* | system"
check "a look-up shows name, key and app count" has $w/store.log "result | probe | http://127.0.0.1:8003/ | ok | Third party | .* | 2"
check "it adds what was looked up" has $w/store.log "result | add | http://127.0.0.1:8003/ | ok"
check "the new repository's apps arrive" has $w/store.log "STORE app | net.example.third | r[0-9a-f]* | Third .* | available"
check "it installs an app" has $w/store.log "result | install | net.example.third | ok"
check "a refused required permission cancels the install" has $w/store.log "result | install | net.example.needy | error | Needy needs the network permission"
check "it uninstalls an app" has $w/store.log "result | uninstall | net.example.third | ok"
check "it cannot uninstall itself" has $w/store.log "result | uninstall | dev.meow.storetest | error | .*running\|part of the system"
check "it asks for an app to be opened" has $w/store.log "result | open | org.example.hello | ok"
check "an unknown repository is an error" has $w/store.log "result | enable | rnone | error"
check "nothing is added without the key that was shown" has $w/store.log "result | add | http://127.0.0.1:8003/ | error"
check "a repository already there is not looked up again" has $w/store.log "result | probe | http://127.0.0.1:8002/ | error | this repository is already"
check "a public http address is refused" has $w/store.log "result | probe | http://8.8.8.8/ | error"
ctl repo
c=$(sed -n 's/^REPO  \(r[0-9a-f]*\)  Third party.*/\1/p' $w/out)
check "the repository the Store app added is on the list" test -n "$c"

# Removing a repository takes its installed apps along.
ctl repo remove meow
check "the built-in store cannot be removed" has $w/out "part of the system"
ctl repo remove "$b"; ctl list
check "a removed repository's apps are gone" lacks $w/out "org.example.hello"
check "and their records" test ! -e /data/appd/installed/org.example.hello
# A record whose repository vanished behind appd's back is not loaded.
ctl install net.example.third all
stop
printf '{"repos":[]}' > /data/appd/repos.json
start
ctl list
check "an app whose repository is gone is not loaded" lacks $w/out "net.example.third"
check "and that is logged" has $w/appd.log "net.example.third: the repository it came from"
stop
kill $servers 2>/dev/null || true
echo "$fails failure(s); logs in $w (appd.log, store.log)"
[ "$fails" = 0 ] && [ -z "${VERBOSE:-}" ] || { echo "---- appd.log"; tail -60 $w/appd.log; echo "---- store.log"; cat $w/store.log; exit 1; }
