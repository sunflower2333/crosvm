#!/system/bin/sh
# Hold the physical panel at the guest's 165 Hz for the lifetime of the VM.
# OPlus casts a 120 Hz window vote for the DroidVM activity (classified as a
# game) which outranks the scanout layer's 165 Hz frame-rate vote, so the only
# way to reach 165 is SurfaceFlinger's fixed-mode override (transaction 1035).
# On VM exit the override is cleared (invalid id) and DisplayManager re-pushes
# its own policy. Usage (root): droidvm-refresh165-hold.sh [crosvm-pid]
LOG=/data/local/tmp/droidvm-refresh165-hold.log
pid=${1:-$(pidof crosvm)}
[ -n "$pid" ] || { echo "$(date) no crosvm" >> $LOG; exit 1; }
disp=$(dumpsys SurfaceFlinger --display-id 2>/dev/null | sed -n 's/^Display \([0-9]*\) .*/\1/p' | head -1)
[ -n "$disp" ] || disp=$(dumpsys SurfaceFlinger | sed -n 's/^Display \([0-9]*\) (physical.*/\1/p' | head -1)
mode=$(dumpsys SurfaceFlinger | grep -oE '\{id=[0-9]+, hwcId=[0-9]+, resolution=[0-9]+x[0-9]+, vsyncRate=165\.00 Hz' | head -1 | sed 's/{id=\([0-9]*\),.*/\1/')
[ -n "$mode" ] && [ -n "$disp" ] || { echo "$(date) no 165 Hz mode (disp=$disp mode=$mode)" >> $LOG; exit 1; }
apply() { service call SurfaceFlinger 1035 i32 $mode i64 $disp >/dev/null 2>&1; }
apply
echo "$(date) hold pid=$pid display=$disp mode=$mode" >> $LOG
n=0
while kill -0 $pid 2>/dev/null; do
  sleep 5
  n=$((n+1))
  # Re-assert every minute in case something cleared the override.
  [ $((n % 12)) -eq 0 ] && apply
done
service call SurfaceFlinger 1035 i32 99 i64 $disp >/dev/null 2>&1
peak=$(settings get system peak_refresh_rate)
settings put system peak_refresh_rate 120.00001; settings put system peak_refresh_rate "$peak"
echo "$(date) released (crosvm $pid exited)" >> $LOG
