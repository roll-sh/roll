#!/bin/bash
# Runs the node instances' user data (deploy/search/aws/roll-search.yaml, NodeLaunchTemplate) on
# Amazon Linux 2023 against loop devices, inside a privileged amazonlinux:2023 container started
# by scripts/search-aws-rehearsal.sh. Never touches the host's disks: the user data only sees
# the devices listed in $DISKS (an lsblk shim lists those and nothing else, loop devices shown
# as disks with the MODEL an instance would report), and only ever formats the one it picks.
#
#   userdata-test.sh <user data script> <scratch dir>
#
# Cases: the Nitro link (/dev/xvdb -> the volume, from amazon-ec2-utils); no link, with an
# instance store disk listed before the EBS volume; an already formatted volume (kept); no data
# volume at all (powers off, never configures the ECS agent). Then the ECS agent's systemd
# unit from AL2023's own ecs-init package must order it after cloud-final (user data).
set -euo pipefail
USERDATA=$1
W=$2
dnf -q -y install xfsprogs util-linux cpio >/dev/null

# Loop devices are the host kernel's: detach every one backed by our files, whatever happens.
cleanup() {
  umount /var/lib/roll-search 2>/dev/null || true
  for f in "$W"/*.img; do
    [ -e "$f" ] || continue
    for l in $(losetup -n -O NAME -j "$f"); do losetup -d "$l" || true; done
    rm -f "$f"
  done
}
trap cleanup EXIT

loop() { # file size -> a loop device node backed by it
  truncate -s "$2" "$W/$1.img"
  local l
  l=$(losetup -f)
  [ -b "$l" ] || mknod "$l" b 7 "${l#/dev/loop}"
  losetup ${3:-} "$l" "$W/$1.img"
  echo "$l"
}

# The instance's disks: a root disk with a partition (like the AMI's), an instance store disk
# (blank, as on m7gd/r7gd), the EBS data volume (blank).
truncate -s 2G "$W/root.img"
printf 'label: gpt\n,,L\n' | sfdisk -q "$W/root.img"
ROOT=$(loop root 2G -P)
STORE=$(loop store 1G)
DATA=$(loop data 1G)

mkdir -p /shim/model /etc/ecs
cat > /shim/lsblk <<'EOF'
#!/bin/bash
# The instance's view: only $DISKS exist, and they are disks with the model a Nitro instance shows.
if [ "$*" = "-dnpo NAME,TYPE" ]; then for d in $DISKS; do echo "$d disk"; done; exit 0; fi
if [ "$1" = "-dno" ] && [ "$2" = "MODEL" ]; then cat "/shim/model/$(basename "$3")" 2>/dev/null || echo; exit 0; fi
exec /usr/bin/lsblk "$@"
EOF
cat > /shim/poweroff <<'EOF'
#!/bin/bash
touch /shim/powered-off
EOF
chmod +x /shim/lsblk /shim/poweroff
echo "Amazon Elastic Block Store" > "/shim/model/$(basename "$ROOT")"
echo "Amazon EC2 NVMe Instance Storage" > "/shim/model/$(basename "$STORE")"
echo "Amazon Elastic Block Store" > "/shim/model/$(basename "$DATA")"

fail() { echo "userdata: FAILED: $*" >&2; exit 1; }
reset() {
  umount /var/lib/roll-search 2>/dev/null || true
  rm -rf /var/lib/roll-search /dev/xvdb /shim/powered-off
  : > /etc/fstab
  : > /etc/ecs/ecs.config
}
run() { # extra PATH dirs first; returns the user data's exit status
  local status=0
  env PATH="/shim:$PATH" DISKS="$DISKS" bash "$USERDATA" > "$W/out" 2>&1 || status=$?
  return $status
}
expect_mounted() { # device
  findmnt -n -o SOURCE,FSTYPE /var/lib/roll-search | grep -qx "$1 xfs" || fail "$(findmnt /var/lib/roll-search || echo 'not mounted'); want $1 xfs"
  [ "$(stat -c %u:%g /var/lib/roll-search)" = 10001:10001 ] || fail "/var/lib/roll-search is $(stat -c %u:%g /var/lib/roll-search), want 10001:10001"
  local uuid
  uuid=$(blkid -s UUID -o value "$1")
  grep -qx "UUID=$uuid /var/lib/roll-search xfs noatime,nofail 0 2" /etc/fstab || fail "fstab: $(cat /etc/fstab)"
  [ "$(grep -c roll-search /etc/fstab)" = 1 ] || fail "fstab has more than one entry: $(cat /etc/fstab)"
  grep -qx "ECS_CLUSTER=roll-test-cluster" /etc/ecs/ecs.config || fail "ecs.config: $(cat /etc/ecs/ecs.config)"
  grep -qx "ECS_ENABLE_SPOT_INSTANCE_DRAINING=true" /etc/ecs/ecs.config || fail "ecs.config: no Spot draining"
  grep -qx "ECS_CONTAINER_STOP_TIMEOUT=120s" /etc/ecs/ecs.config || fail "ecs.config: no stop timeout"
  [ ! -e /shim/powered-off ] || fail "powered off"
  # The fstab entry mounts it again at the next boot.
  umount /var/lib/roll-search && mount -a && findmnt -n /var/lib/roll-search >/dev/null || fail "mount -a from fstab"
}

# 1. Nitro: amazon-ec2-utils' udev rule links the block device mapping name to the NVMe device.
reset
DISKS="$ROOT $STORE $DATA"
ln -s "$DATA" /dev/xvdb
run || fail "with /dev/xvdb: exit $?: $(cat "$W/out")"
expect_mounted "$DATA"
blkid -p "$STORE" >/dev/null 2>&1 && fail "the instance store disk was formatted"
echo "userdata: /dev/xvdb link: mounted $DATA (xfs, 10001, fstab by UUID), ECS configured"

# 2. No link: the blank EBS disk, never the blank instance store disk listed before it.
reset
wipefs -q -a "$DATA"
run || fail "without the link: exit $?: $(cat "$W/out")"
expect_mounted "$DATA"
blkid -p "$STORE" >/dev/null 2>&1 && fail "the instance store disk was formatted"
echo "userdata: no link: picked the EBS disk over the instance store"

# 3. A formatted volume is kept (not formatted again).
touch /var/lib/roll-search/keep
umount /var/lib/roll-search
reset
ln -s "$DATA" /dev/xvdb
run || fail "formatted volume: exit $?: $(cat "$W/out")"
expect_mounted "$DATA"
[ -e /var/lib/roll-search/keep ] || fail "the volume was formatted again"
echo "userdata: a formatted volume keeps its files"

# 4. No data volume: power off without configuring the ECS agent (sleep shimmed: 120 tries).
umount /var/lib/roll-search
reset
DISKS="$ROOT $STORE"
printf '#!/bin/bash\n' > /shim/sleep && chmod +x /shim/sleep
if run; then fail "succeeded without a data volume"; fi
rm /shim/sleep
[ -e /shim/powered-off ] || fail "didn't power off without a data volume: $(cat "$W/out")"
[ ! -s /etc/ecs/ecs.config ] || fail "configured the ECS agent without a data volume"
grep -q "no data volume" "$W/out" || fail "no message: $(cat "$W/out")"
blkid -p "$STORE" >/dev/null 2>&1 && fail "the instance store disk was formatted"
echo "userdata: no data volume: powered off, ECS agent not configured"

# 5. The ECS agent (AL2023's ecs-init) starts after cloud-final, which runs the user data: the
# volume is mounted before any task can be placed on the instance.
mkdir -p "$W/rpm"
dnf -q -y install --downloadonly --downloaddir="$W/rpm" ecs-init >/dev/null 2>&1 || true
rpm=$(ls "$W"/rpm/ecs-init-*.rpm 2>/dev/null | head -1)
[ -n "$rpm" ] || fail "couldn't download AL2023's ecs-init package"
(cd "$W/rpm" && rpm2cpio "$rpm" | cpio -idm --quiet)
unit=$(ls "$W"/rpm/usr/lib/systemd/system/ecs.service)
grep -qx "After=cloud-final.service" "$unit" || fail "ecs.service is not ordered after cloud-final: $(cat "$unit")"
grep -qx "EnvironmentFile=-/etc/ecs/ecs.config" "$unit" || fail "ecs.service doesn't read /etc/ecs/ecs.config"
echo "userdata: $(basename "$rpm"): ecs.service runs After=cloud-final.service"
rm -rf "$W/rpm"
echo "userdata: all green"
