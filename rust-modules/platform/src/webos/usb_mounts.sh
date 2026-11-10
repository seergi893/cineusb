# Executed by Homebrew Channel's root service. app_id is supplied by jail_repair.rs.
[ "$(id -u)" = 0 ] || exit 1
jail=$(readlink -f "/var/palm/jail/$app_id") || exit 1
[ -d "$jail/tmp" ] && [ ! -L "$jail/tmp" ] || exit 1
case "$jail" in
    /var/palm/jail/"$app_id"|/mnt/lg/user/var/palm/jail/"$app_id") ;;
    *) exit 1 ;;
esac
bridge="$jail/tmp/cineusb-volumes"
[ ! -L "$bridge" ] || exit 1
mkdir -p -m 755 "$bridge" || exit 1
[ "$(stat -c '%u:%a' "$bridge")" = '0:755' ] || exit 1

# Keep the Linux mount ID in each directory name. An unplugged partition must not survive
# under its old name, even if webOS gives a replacement disk the same /tmp/usb path.
table=$(cat /proc/self/mountinfo) || exit 1
live=$(printf '%s\n' "$table" | awk '
    # webOS can mount a partition directly at /tmp/usb/<label> or at
    # /tmp/usb/<device>/<partition>. Neither path needs a trailing slash.
    $5 ~ /^\/tmp\/usb\/[^/]+(\/[^/]+)?$/ && $0 !~ / - (tmpfs|ramfs) / { print $1, $5 }
')
bridge_is_readonly() {
    awk -v p="$1" '$5 == p && $6 ~ /(^|,)ro(,|$)/ { found=1 } END { exit !found }' /proc/self/mountinfo
}
for target in "$bridge"/usb-*; do
    [ -d "$target" ] && [ ! -L "$target" ] || continue
    mount_id=${target##*/usb-}
    mount_id=${mount_id%%-*}
    if ! printf '%s\n' "$live" | awk -v id="$mount_id" '$1 == id { found=1 } END { exit !found }'; then
        if awk -v p="$target" '$5 == p { found=1 } END { exit !found }' /proc/self/mountinfo; then
            umount -l "$target" || exit 1
        fi
        rmdir "$target" || exit 1
    fi
done

printf '%s\n' "$live" | while IFS=' ' read -r mount_id escaped; do
    [ -n "$escaped" ] || continue
    source=$(printf '%b' "$escaped")
    [ -d "$source" ] && [ "$(readlink -f "$source")" = "$source" ] || continue
    label=$(printf '%s' "${source##*/}" | LC_ALL=C tr -c 'a-zA-Z0-9._-' '_')
    target="$bridge/usb-$mount_id-$label"
    [ ! -L "$target" ] || exit 1
    mkdir -p "$target" || exit 1
    if ! awk -v p="$target" '$5 == p { found=1 } END { exit !found }' /proc/self/mountinfo; then
        mount --bind "$source" "$target" || exit 1
    fi
    if ! bridge_is_readonly "$target" && {
        ! mount -o remount,bind,ro,nosuid,nodev,noexec "$target" || ! bridge_is_readonly "$target";
    }; then
        umount -l "$target"
        exit 1
    fi
done || exit 1
printf CINEUSB_MOUNTS_OK
