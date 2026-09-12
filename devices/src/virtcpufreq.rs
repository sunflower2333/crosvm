// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use base::sched_attr;
use base::sched_setattr;
use base::warn;
use base::Error;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;
use snapshot::AnySnapshot;
use sync::Mutex;

use crate::pci::CrosvmDeviceId;
use crate::BusAccessInfo;
use crate::BusDevice;
use crate::DeviceId;
use crate::Suspendable;

const CPUFREQ_GOV_SCALE_FACTOR_DEFAULT: u32 = 100;
const CPUFREQ_GOV_SCALE_FACTOR_SCHEDUTIL: u32 = 80;
const SCHED_SCALE_CAPACITY: u32 = 1024;

const SCHED_FLAG_RESET_ON_FORK: u64 = 0x1;
const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
const SCHED_FLAG_UTIL_CLAMP_MIN: u64 = 0x20;
const SCHED_FLAG_UTIL_CLAMP_MAX: u64 = 0x40;

const SCHED_FLAG_KEEP_ALL: u64 = SCHED_FLAG_KEEP_POLICY | SCHED_FLAG_KEEP_PARAMS;

#[derive(Serialize, Deserialize)]
pub struct VirtCpufreq {
    pcpu_fmax: u32,
    pcpu_capacity: u32,
    vcpu_fmax: u32,
    vcpu_capacity: u32,
    vcpu_relative_capacity: u32,
    pcpu: u32,
    util_factor: u32,
    bad_util_logged: bool,
    sched_setattr_fail_logged: bool,
}

fn get_cpu_info(cpu_id: u32, property: &str) -> Result<u32, Error> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu_id}/{property}");
    std::fs::read_to_string(path)?
        .trim()
        .parse()
        .map_err(|_| Error::new(libc::EINVAL))
}

fn get_cpu_info_str(cpu_id: u32, property: &str) -> Result<String, Error> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu_id}/{property}");
    std::fs::read_to_string(path).map_err(|_| Error::new(libc::EINVAL))
}

fn get_cpu_capacity(cpu_id: u32) -> Result<u32, Error> {
    get_cpu_info(cpu_id, "cpu_capacity")
}

fn get_cpu_maxfreq_khz(cpu_id: u32) -> Result<u32, Error> {
    get_cpu_info(cpu_id, "cpufreq/cpuinfo_max_freq")
}

fn get_cpu_curfreq_khz(cpu_id: u32) -> Result<u32, Error> {
    get_cpu_info(cpu_id, "cpufreq/scaling_cur_freq")
}

fn handle_read_err(err: Error) -> String {
    warn!("Unable to get cpufreq governor, using 100% default util factor. Err: {:?}", err);
    "unknown_governor".to_string()
}

fn get_cpu_util_factor(cpu_id: u32) -> Result<u32, Error> {
    let gov = get_cpu_info_str(cpu_id, "cpufreq/scaling_governor").unwrap_or_else(handle_read_err);
    match gov.trim() {
        "schedutil" => Ok(CPUFREQ_GOV_SCALE_FACTOR_SCHEDUTIL),
        _ => Ok(CPUFREQ_GOV_SCALE_FACTOR_DEFAULT),
    }
}

impl VirtCpufreq {
    pub fn new(pcpu: u32, cpu_capacity: u32, cpu_fmax: u32) -> Self {
        let util_factor = get_cpu_util_factor(pcpu).expect("Error getting util factor");
        let pcpu_capacity = get_cpu_capacity(pcpu).expect("Error reading capacity");
        let pcpu_fmax = get_cpu_maxfreq_khz(pcpu).expect("Error reading max freq");
        let vcpu_relative_capacity = u32::try_from((u64::from(cpu_capacity) * u64::from(pcpu_fmax) / u64::from(cpu_fmax))).unwrap();

        VirtCpufreq {
            pcpu_fmax,
            pcpu_capacity,
            vcpu_fmax: cpu_fmax,
            vcpu_capacity: cpu_capacity,
            vcpu_relative_capacity,
            pcpu,
            util_factor,
            bad_util_logged: false,
            sched_setattr_fail_logged: false,
        }
    }
}

impl BusDevice for VirtCpufreq {
    fn device_id(&self) -> DeviceId {
        CrosvmDeviceId::VirtCpufreq.into()
    }

    fn debug_label(&self) -> String {
        "VirtCpufreq Device".to_owned()
    }

    fn read(&mut self, _info: BusAccessInfo, data: &mut [u8]) {
        if data.len() != std::mem::size_of::<u32>() {
            warn!(
                "{}: unsupported read length {}, only support 4bytes read",
                self.debug_label(),
                data.len()
            );
            return;
        }
        // TODO(davidai): Evaluate opening file and re-reading the same fd.
        let freq = match get_cpu_curfreq_khz(self.pcpu) {
            Ok(freq) => {
                u32::try_from((u64::from(freq) * u64::from(self.pcpu_capacity) / u64::from(self.vcpu_relative_capacity))).unwrap()
            },
            Err(e) => panic!("{}: Error reading freq: {}", self.debug_label(), e),
        };

        let freq_arr = freq.to_ne_bytes();
        data.copy_from_slice(&freq_arr);
    }

    fn write(&mut self, _info: BusAccessInfo, data: &[u8]) {
        let freq: u32 = match data.try_into().map(u32::from_ne_bytes) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "{}: unsupported write length {}, only support 4bytes write",
                    self.debug_label(),
                    e
                );
                return;
            }
        };

        // Util margin depends on the cpufreq governor on the host
        let cpu_cap_scaled = self.vcpu_capacity * self.util_factor / CPUFREQ_GOV_SCALE_FACTOR_DEFAULT;
        let mut util = u32::try_from(u64::from(cpu_cap_scaled) * u64::from(freq) / u64::from(self.vcpu_fmax)).unwrap();

        let mut sched_attr = sched_attr::default();
        sched_attr.sched_flags =
            SCHED_FLAG_KEEP_ALL | SCHED_FLAG_UTIL_CLAMP_MIN | SCHED_FLAG_UTIL_CLAMP_MAX | SCHED_FLAG_RESET_ON_FORK;

        if (util > 1024) {
            if (!self.bad_util_logged) {
                warn!("{}: Out of range util value: {}, freq:{}, fmax:{}", self.debug_label(), util, freq, self.vcpu_fmax);
                self.bad_util_logged = true;
            }
            util = 1024;
        }

        sched_attr.sched_util_min = util;

        if self.vcpu_fmax != self.pcpu_fmax {
            sched_attr.sched_util_max = util;
        } else {
            sched_attr.sched_util_max = 1024;
        }

        if let Err(e) = sched_setattr(0, &mut sched_attr, 0) {
            // The logging above should catch out of range util values, if we're still unable
            // to successfully call sched_setattr, there might be intermittent permission issues.
            if (!self.sched_setattr_fail_logged) {
                warn!("{}: Error setting util:{}, Error: {}", self.debug_label(), util, e);
                self.sched_setattr_fail_logged = true;
            }
        }
    }
}

impl Suspendable for VirtCpufreq {
    // Device only active through MMIO writes. Vcpus are frozen before the device tries to sleep,
    // so the device will not be active at time of calling function.
    fn sleep(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn wake(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn snapshot(&mut self) -> anyhow::Result<AnySnapshot> {
        AnySnapshot::to_any(&self).context("failed to serialize")
    }

    fn restore(&mut self, data: AnySnapshot) -> anyhow::Result<()> {
        let deser: Self = AnySnapshot::from_any(data).context("failed to deserialize")?;
        *self = deser;
        Ok(())
    }
}
