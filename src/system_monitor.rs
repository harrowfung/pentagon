use metrics::gauge;
use std::time::Duration;
use sysinfo::{Disks, System};
use tokio::time;

pub async fn start_system_monitor() {
    // Describe metrics

    tokio::spawn(async move {
        let mut system = System::new_all();
        let mut disks = Disks::new_with_refreshed_list();
        let mut interval = time::interval(Duration::from_secs(5));

        loop {
            interval.tick().await;

            system.refresh_memory();
            system.refresh_cpu_all();
            disks.refresh(true);

            // Memory
            gauge!("system_memory_used_bytes").set(system.used_memory() as f64);
            gauge!("system_memory_total_bytes").set(system.total_memory() as f64);

            // CPU
            let global_cpu = system.global_cpu_usage();
            gauge!("system_cpu_usage_percent").set(global_cpu as f64);

            // Disk
            // We'll aggregate all disks for a simple overview
            let mut total_free = 0;
            let mut total_space = 0;
            for disk in &disks {
                total_free += disk.available_space();
                total_space += disk.total_space();
            }
            gauge!("system_disk_free_bytes").set(total_free as f64);
            gauge!("system_disk_total_bytes").set(total_space as f64);
        }
    });
}
