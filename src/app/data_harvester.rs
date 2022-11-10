//! This is the main file to house data collection functions.

use std::time::Instant;

use futures::join;

#[cfg(target_os = "linux")]
use fxhash::FxHashMap;

#[cfg(feature = "battery")]
use starship_battery::{Battery, Manager};

#[cfg(not(target_os = "linux"))]
use sysinfo::{System, SystemExt};

use super::DataFilters;
use crate::app::layout_manager::UsedWidgets;

#[cfg(feature = "nvidia")]
pub mod nvidia;

#[cfg(feature = "battery")]
pub mod batteries;
pub mod cpu;
pub mod disks;
pub mod memory;
pub mod network;
pub mod processes;
pub mod temperature;

#[derive(Clone, Debug)]
pub struct Data {
    pub last_collection_time: Instant,
    pub cpu: Option<cpu::CpuHarvest>,
    pub load_avg: Option<cpu::LoadAvgHarvest>,
    pub memory: Option<memory::MemHarvest>,
    pub swap: Option<memory::MemHarvest>,
    pub temperature_sensors: Option<Vec<temperature::TempHarvest>>,
    pub network: Option<network::NetworkHarvest>,
    pub list_of_processes: Option<Vec<processes::ProcessHarvest>>,
    pub disks: Option<Vec<disks::DiskHarvest>>,
    pub io: Option<disks::IoHarvest>,
    #[cfg(feature = "battery")]
    pub list_of_batteries: Option<Vec<batteries::BatteryHarvest>>,
    #[cfg(feature = "zfs")]
    pub arc: Option<memory::MemHarvest>,
    #[cfg(feature = "gpu")]
    pub gpu: Option<Vec<(String, memory::MemHarvest)>>,
}

impl Default for Data {
    fn default() -> Self {
        Data {
            last_collection_time: Instant::now(),
            cpu: None,
            load_avg: None,
            memory: None,
            swap: None,
            temperature_sensors: None,
            list_of_processes: None,
            disks: None,
            io: None,
            network: None,
            #[cfg(feature = "battery")]
            list_of_batteries: None,
            #[cfg(feature = "zfs")]
            arc: None,
            #[cfg(feature = "gpu")]
            gpu: None,
        }
    }
}

impl Data {
    pub fn cleanup(&mut self) {
        self.io = None;
        self.temperature_sensors = None;
        self.list_of_processes = None;
        self.disks = None;
        self.memory = None;
        self.swap = None;
        self.cpu = None;
        self.load_avg = None;

        if let Some(network) = &mut self.network {
            network.first_run_cleanup();
        }
        #[cfg(feature = "zfs")]
        {
            self.arc = None;
        }
        #[cfg(feature = "gpu")]
        {
            self.gpu = None;
        }
    }
}

#[derive(Debug)]
pub struct DataCollector {
    pub data: Data,
    #[cfg(not(target_os = "linux"))]
    sys: System,
    previous_cpu_times: Vec<(cpu::PastCpuWork, cpu::PastCpuTotal)>,
    previous_average_cpu_time: Option<(cpu::PastCpuWork, cpu::PastCpuTotal)>,
    #[cfg(target_os = "linux")]
    pid_mapping: FxHashMap<crate::Pid, processes::PrevProcDetails>,
    #[cfg(target_os = "linux")]
    prev_idle: f64,
    #[cfg(target_os = "linux")]
    prev_non_idle: f64,
    mem_total_kb: u64,
    temperature_type: temperature::TemperatureType,
    use_current_cpu_total: bool,
    last_collection_time: Instant,
    total_rx: u64,
    total_tx: u64,
    show_average_cpu: bool,
    widgets_to_harvest: UsedWidgets,
    #[cfg(feature = "battery")]
    battery_manager: Option<Manager>,
    #[cfg(feature = "battery")]
    battery_list: Option<Vec<Battery>>,
    filters: DataFilters,

    #[cfg(target_family = "unix")]
    user_table: self::processes::UserTable,
}

impl DataCollector {
    pub fn new(filters: DataFilters) -> Self {
        DataCollector {
            data: Data::default(),
            #[cfg(not(target_os = "linux"))]
            sys: System::new_with_specifics(sysinfo::RefreshKind::new()),
            previous_cpu_times: vec![],
            previous_average_cpu_time: None,
            #[cfg(target_os = "linux")]
            pid_mapping: FxHashMap::default(),
            #[cfg(target_os = "linux")]
            prev_idle: 0_f64,
            #[cfg(target_os = "linux")]
            prev_non_idle: 0_f64,
            mem_total_kb: 0,
            temperature_type: temperature::TemperatureType::Celsius,
            use_current_cpu_total: false,
            last_collection_time: Instant::now(),
            total_rx: 0,
            total_tx: 0,
            show_average_cpu: false,
            widgets_to_harvest: UsedWidgets::default(),
            #[cfg(feature = "battery")]
            battery_manager: None,
            #[cfg(feature = "battery")]
            battery_list: None,
            filters,
            #[cfg(target_family = "unix")]
            user_table: Default::default(),
        }
    }

    pub fn init(&mut self) {
        #[cfg(target_os = "linux")]
        {
            futures::executor::block_on(self.initialize_memory_size());
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.sys.refresh_memory();
            self.mem_total_kb = self.sys.total_memory();

            // TODO: Would be good to get this and network list running on a timer instead...?
            // Refresh components list once...
            if self.widgets_to_harvest.use_temp {
                self.sys.refresh_components_list();
            }

            // Refresh network list once...
            if cfg!(target_os = "windows") && self.widgets_to_harvest.use_net {
                self.sys.refresh_networks_list();
            }

            if cfg!(target_os = "freebsd") && self.widgets_to_harvest.use_cpu {
                self.sys.refresh_cpu();
            }

            // Refresh disk list once...
            if cfg!(target_os = "freebsd") && self.widgets_to_harvest.use_disk {
                self.sys.refresh_disks_list();
            }
        }

        #[cfg(feature = "battery")]
        {
            if self.widgets_to_harvest.use_battery {
                if let Ok(battery_manager) = Manager::new() {
                    if let Ok(batteries) = battery_manager.batteries() {
                        let battery_list: Vec<Battery> = batteries.filter_map(Result::ok).collect();
                        if !battery_list.is_empty() {
                            self.battery_list = Some(battery_list);
                            self.battery_manager = Some(battery_manager);
                        }
                    }
                }
            }
        }

        futures::executor::block_on(self.update_data());

        std::thread::sleep(std::time::Duration::from_millis(250));

        self.data.cleanup();

        // trace!("Enabled widgets to harvest: {:#?}", self.widgets_to_harvest);
    }

    #[cfg(target_os = "linux")]
    async fn initialize_memory_size(&mut self) {
        self.mem_total_kb = if let Ok(mem) = heim::memory::memory().await {
            mem.total().get::<heim::units::information::kilobyte>()
        } else {
            1
        };
    }

    pub fn set_data_collection(&mut self, used_widgets: UsedWidgets) {
        self.widgets_to_harvest = used_widgets;
    }

    pub fn set_temperature_type(&mut self, temperature_type: temperature::TemperatureType) {
        self.temperature_type = temperature_type;
    }

    pub fn set_use_current_cpu_total(&mut self, use_current_cpu_total: bool) {
        self.use_current_cpu_total = use_current_cpu_total;
    }

    pub fn set_show_average_cpu(&mut self, show_average_cpu: bool) {
        self.show_average_cpu = show_average_cpu;
    }

    pub async fn update_data(&mut self) {
        #[cfg(not(target_os = "linux"))]
        {
            if self.widgets_to_harvest.use_proc || self.widgets_to_harvest.use_cpu {
                self.sys.refresh_cpu();
            }
            if self.widgets_to_harvest.use_proc {
                self.sys.refresh_processes();
            }
            if self.widgets_to_harvest.use_temp {
                self.sys.refresh_components();
            }
            if cfg!(target_os = "windows") && self.widgets_to_harvest.use_net {
                self.sys.refresh_networks();
            }
            if cfg!(target_os = "freebsd") && self.widgets_to_harvest.use_disk {
                self.sys.refresh_disks();
            }
            if cfg!(target_os = "freebsd") && self.widgets_to_harvest.use_mem {
                self.sys.refresh_memory();
            }
        }

        let current_instant = std::time::Instant::now();

        // CPU
        if self.widgets_to_harvest.use_cpu {
            #[cfg(not(target_os = "freebsd"))]
            {
                if let Ok(cpu_data) = cpu::get_cpu_data_list(
                    self.show_average_cpu,
                    &mut self.previous_cpu_times,
                    &mut self.previous_average_cpu_time,
                )
                .await
                {
                    self.data.cpu = Some(cpu_data);
                }
            }
            #[cfg(target_os = "freebsd")]
            {
                if let Ok(cpu_data) = cpu::get_cpu_data_list(
                    &self.sys,
                    self.show_average_cpu,
                    &mut self.previous_cpu_times,
                    &mut self.previous_average_cpu_time,
                )
                .await
                {
                    self.data.cpu = Some(cpu_data);
                }
            }

            #[cfg(target_family = "unix")]
            {
                // Load Average
                if let Ok(load_avg_data) = cpu::get_load_avg().await {
                    self.data.load_avg = Some(load_avg_data);
                }
            }
        }

        // Batteries
        #[cfg(feature = "battery")]
        {
            if let Some(battery_manager) = &self.battery_manager {
                if let Some(battery_list) = &mut self.battery_list {
                    self.data.list_of_batteries =
                        Some(batteries::refresh_batteries(battery_manager, battery_list));
                }
            }
        }

        if self.widgets_to_harvest.use_proc {
            if let Ok(process_list) = {
                #[cfg(target_os = "linux")]
                {
                    processes::get_process_data(
                        &mut self.prev_idle,
                        &mut self.prev_non_idle,
                        &mut self.pid_mapping,
                        self.use_current_cpu_total,
                        current_instant
                            .duration_since(self.last_collection_time)
                            .as_secs(),
                        self.mem_total_kb,
                        &mut self.user_table,
                    )
                }
                #[cfg(not(target_os = "linux"))]
                {
                    #[cfg(target_family = "unix")]
                    {
                        processes::get_process_data(
                            &self.sys,
                            self.use_current_cpu_total,
                            self.mem_total_kb,
                            &mut self.user_table,
                        )
                    }
                    #[cfg(not(target_family = "unix"))]
                    {
                        processes::get_process_data(
                            &self.sys,
                            self.use_current_cpu_total,
                            self.mem_total_kb,
                        )
                    }
                }
            } {
                self.data.list_of_processes = Some(process_list);
            }
        }

        if self.widgets_to_harvest.use_temp {
            #[cfg(not(target_os = "linux"))]
            {
                if let Ok(data) = temperature::get_temperature_data(
                    &self.sys,
                    &self.temperature_type,
                    &self.filters.temp_filter,
                ) {
                    self.data.temperature_sensors = data;
                }
            }

            #[cfg(target_os = "linux")]
            {
                if let Ok(data) = temperature::get_temperature_data(
                    &self.temperature_type,
                    &self.filters.temp_filter,
                ) {
                    self.data.temperature_sensors = data;
                }
            }
        }

        let network_data_fut = {
            #[cfg(any(target_os = "windows", target_os = "freebsd"))]
            {
                network::get_network_data(
                    &self.sys,
                    self.last_collection_time,
                    &mut self.total_rx,
                    &mut self.total_tx,
                    current_instant,
                    self.widgets_to_harvest.use_net,
                    &self.filters.net_filter,
                )
            }
            #[cfg(not(any(target_os = "windows", target_os = "freebsd")))]
            {
                network::get_network_data(
                    self.last_collection_time,
                    &mut self.total_rx,
                    &mut self.total_tx,
                    current_instant,
                    self.widgets_to_harvest.use_net,
                    &self.filters.net_filter,
                )
            }
        };
        let mem_data_fut = {
            #[cfg(not(target_os = "freebsd"))]
            {
                memory::get_mem_data(
                    self.widgets_to_harvest.use_mem,
                    self.widgets_to_harvest.use_gpu,
                )
            }
            #[cfg(target_os = "freebsd")]
            {
                memory::get_mem_data(
                    &self.sys,
                    self.widgets_to_harvest.use_mem,
                    self.widgets_to_harvest.use_gpu,
                )
            }
        };
        let disk_data_fut = disks::get_disk_usage(
            self.widgets_to_harvest.use_disk,
            &self.filters.disk_filter,
            &self.filters.mount_filter,
        );
        let disk_io_usage_fut = disks::get_io_usage(self.widgets_to_harvest.use_disk);

        let (net_data, mem_res, disk_res, io_res) = join!(
            network_data_fut,
            mem_data_fut,
            disk_data_fut,
            disk_io_usage_fut,
        );

        if let Ok(net_data) = net_data {
            if let Some(net_data) = &net_data {
                self.total_rx = net_data.total_rx;
                self.total_tx = net_data.total_tx;
            }
            self.data.network = net_data;
        }

        if let Ok(memory) = mem_res.ram {
            self.data.memory = memory;
        }

        if let Ok(swap) = mem_res.swap {
            self.data.swap = swap;
        }

        #[cfg(feature = "zfs")]
        if let Ok(arc) = mem_res.arc {
            self.data.arc = arc;
        }

        #[cfg(feature = "gpu")]
        if let Ok(gpu) = mem_res.gpus {
            self.data.gpu = gpu;
        }

        if let Ok(disks) = disk_res {
            self.data.disks = disks;
        }

        if let Ok(io) = io_res {
            self.data.io = io;
        }

        // Update time
        self.data.last_collection_time = current_instant;
        self.last_collection_time = current_instant;
    }
}

#[cfg(target_os = "freebsd")]
/// Deserialize [libxo](https://www.freebsd.org/cgi/man.cgi?query=libxo&apropos=0&sektion=0&manpath=FreeBSD+13.1-RELEASE+and+Ports&arch=default&format=html) JSON data
fn deserialize_xo<T>(key: &str, data: &[u8]) -> Result<T, std::io::Error>
where
    T: serde::de::DeserializeOwned,
{
    let mut value: serde_json::Value = serde_json::from_slice(data)?;
    value
        .as_object_mut()
        .and_then(|map| map.remove(key))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "key not found"))
        .and_then(|val| serde_json::from_value(val).map_err(|err| err.into()))
}
