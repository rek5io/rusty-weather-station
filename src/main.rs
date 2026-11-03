#![no_std]
#![no_main]

use alloc::format;
use core::{cell::RefCell, net::Ipv4Addr};
use embassy_sync::{
    blocking_mutex::{NoopMutex, raw::CriticalSectionRawMutex},
    mutex::Mutex,
};
use embassy_time::{Delay, Instant, Timer};
use esp_hal::{
    Blocking,
    analog::adc,
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    i2c::master::I2c,
    spi::master::Spi,
    timer::timg::TimerGroup,
};
use esp_println::println;
use picoserve::response::Json;
use picoserve::routing::{get, get_service};
use serde::Serialize;
use static_cell::StaticCell;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    esp_println::println!("panic: {:?}", info);
    loop {}
}

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

struct State {
    measurement: Measurement,
    ip_address: Option<Ipv4Addr>,
}

#[derive(Debug, Serialize, Clone, Copy, Default)]
struct Measurement {
    temperature: f32,
    pressure: f32,
    humidity: f32,
    air_quality: f32,
    time_millis: u64,
}

static STATE: Mutex<CriticalSectionRawMutex, State> = Mutex::new(State {
    measurement: Measurement {
        temperature: 0.0,
        pressure: 0.0,
        humidity: 0.0,
        air_quality: 0.0,
        time_millis: 0,
    },

    ip_address: None,
});

#[esp_rtos::main]
async fn main(_sp: embassy_executor::Spawner) -> ! {
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64000);

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    let timg0 = TimerGroup::new(peripherals.TIMG0);

    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    let mut led_0 = Output::new(peripherals.GPIO8, Level::High, OutputConfig::default());

    let i2c = I2c::new(peripherals.I2C0, esp_hal::i2c::master::Config::default())
        .unwrap()
        .with_sda(peripherals.GPIO1)
        .with_scl(peripherals.GPIO2);

    static I2C: StaticCell<NoopMutex<RefCell<I2c<'_, Blocking>>>> = StaticCell::new();
    let i2c_bus = embassy_embedded_hal::shared_bus::blocking::i2c::I2cDevice::new(
        I2C.init(NoopMutex::new(RefCell::new(i2c))),
    );

    let mut bme = bme280::i2c::BME280::new_primary(i2c_bus.clone());
    if let Err(e) = bme.init(&mut Delay) {
        esp_println::println!("failed to init bme: {:?}", e);
    }

    let mut display = oled_i2c::Oled::new(
        i2c_bus,
        0x3C,
        oled_i2c::OledConfig::new(oled_i2c::OledKind::Sh1106, 128, 64).with_contrast(0xCF),
    );

    match display.as_mut() {
        Ok(display) => {
            display.display_off().unwrap();
            display.clear().unwrap();
            display.clear_buffer();
            display.display_on().unwrap();
        }

        Err(e) => println!("failed to init display: {:?}", e),
    }

    let spi = Spi::new(peripherals.SPI2, esp_hal::spi::master::Config::default())
        .unwrap()
        .with_miso(peripherals.GPIO5)
        .with_mosi(peripherals.GPIO6)
        .with_sck(peripherals.GPIO4)
        .into_async();

    let cs = Output::new(peripherals.GPIO7, Level::High, OutputConfig::default());

    let mut adc_cfg = adc::AdcConfig::new();
    let mut adc_pin = adc_cfg.enable_pin(peripherals.GPIO3, adc::Attenuation::_11dB);
    let mut adc = adc::Adc::new(peripherals.ADC1, adc_cfg).into_async();

    let w5500_int = Input::new(
        peripherals.GPIO10,
        InputConfig::default().with_pull(Pull::Up),
    );

    let w5500_reset = Output::new(peripherals.GPIO20, Level::High, OutputConfig::default());

    let mut w5500_runner = None;
    let mut net_runner = None;
    let mut wifi_runner = None;
    let mut _wifi_controller = None;
    let mut net_stack = None;
    let fallback_ip = embassy_net::Ipv4Address::new(10, 0, 0, 1);

    let mac_addr = [0xde, 0xad, 0xbe, 0xef, 0x0f, 0x0f];
    let rng = esp_hal::rng::Rng::new();

    static NET_STATE: StaticCell<embassy_net_wiznet::State<8, 8>> = StaticCell::new();
    if let Ok((device, w5500_runner_)) =
        embassy_net_wiznet::new::<8, 8, embassy_net_wiznet::chip::W5500, _, _, _>(
            mac_addr,
            NET_STATE.init(embassy_net_wiznet::State::<8, 8>::new()),
            embedded_hal_bus::spi::ExclusiveDevice::new(spi, cs, Delay).unwrap(),
            w5500_int,
            w5500_reset,
        )
        .await
    {
        w5500_runner = Some(w5500_runner_);

        static STACK: StaticCell<embassy_net::StackResources<8>> = StaticCell::new();
        let (net_stack_, net_runner_) = embassy_net::new(
            device,
            embassy_net::Config::dhcpv4(Default::default()),
            STACK.init(embassy_net::StackResources::new()),
            rng.random() as u64,
        );

        net_stack = Some(net_stack_);
        net_runner = Some(net_runner_);
    }

    let allow_wifi = true;

    if net_stack.is_none() && allow_wifi {
        esp_println::println!("fallback to wifi");

        let wifi_controller = esp_radio::wifi::WifiController::new(
            peripherals.WIFI,
            esp_radio::wifi::ControllerConfig::default().with_initial_config(
                esp_radio::wifi::Config::AccessPoint(
                    esp_radio::wifi::ap::AccessPointConfig::default()
                        .with_ssid(esp_radio::wifi::Ssid::try_from("ESP32-C3").unwrap())
                        .with_authentication(esp_radio::wifi::AuthenticationMethodConfig::Wpa(
                            esp_radio::wifi::Password::try_from("12345678").unwrap(),
                        )),
                ),
            ),
        )
        .unwrap();

        let net_config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
            address: embassy_net::Ipv4Cidr::new(fallback_ip, 24),
            gateway: Some(fallback_ip),
            dns_servers: Default::default(),
        });

        static STACK: StaticCell<embassy_net::StackResources<3>> = StaticCell::new();
        let (net_stack_, net_runner_) = embassy_net::new(
            esp_radio::wifi::Interface::access_point(),
            net_config,
            STACK.init(embassy_net::StackResources::new()),
            rng.random() as u64,
        );

        _wifi_controller = Some(wifi_controller);
        net_stack = Some(net_stack_);
        wifi_runner = Some(net_runner_);
    }

    let w5500_runner_task = async move {
        if let Some(w550_runner) = w5500_runner {
            w550_runner.run().await
        }
    };

    let wifi_runner_present = wifi_runner.is_some();

    let net_runner_task = async move {
        if let Some(mut net_runner) = net_runner {
            esp_println::println!("starting w5500 net runner");
            net_runner.run().await
        } else if let Some(mut net_runner) = wifi_runner {
            esp_println::println!("starting wifi net runner");
            net_runner.run().await
        }
    };

    let net_dhcp_task = async move {
        use edge_nal::UdpBind;

        if !wifi_runner_present {
            return;
        }

        let mut buf = [0u8; 1500];
        let mut gw_buf = [core::net::Ipv4Addr::UNSPECIFIED];
        let buffers = edge_nal_embassy::UdpBuffers::<3, 1024, 1024, 10>::new();
        let unbound_socket = edge_nal_embassy::Udp::new(net_stack.unwrap(), &buffers);
        let mut bound_socket = unbound_socket
            .bind(core::net::SocketAddr::V4(core::net::SocketAddrV4::new(
                core::net::Ipv4Addr::UNSPECIFIED,
                edge_dhcp::io::DEFAULT_SERVER_PORT,
            )))
            .await
            .unwrap();

        loop {
            _ = edge_dhcp::io::server::run(
                &mut edge_dhcp::server::Server::<_, 64>::new_with_et(fallback_ip),
                &edge_dhcp::server::ServerOptions::new(fallback_ip, Some(&mut gw_buf)),
                &mut bound_socket,
                &mut buf,
            )
            .await
            .inspect_err(|e| println!("DHCP server error: {e:?}"));
            Timer::after_millis(500).await;
        }
    };

    let http_server_task = async {
        let Some(net_stack) = net_stack else {
            esp_println::println!("welp, net is not available");
            return;
        };

        let router = picoserve::Router::new()
            .route(
                "/measurement",
                get(async || -> Json<Measurement> { Json(STATE.lock().await.measurement) }),
            )
            .route(
                "/",
                get_service(picoserve::response::File::html(include_str!("index.html"))),
            );

        let config = picoserve::Config::default();

        static RX: StaticCell<[u8; 4096]> = StaticCell::new();
        static TX: StaticCell<[u8; 4096]> = StaticCell::new();
        static HTTP_BUFFER: StaticCell<[u8; 4096]> = StaticCell::new();
        let http_buffer = HTTP_BUFFER.init([0u8; 4096]);
        let rx_buffer = RX.init([0u8; 4096]);
        let tx_buffer = TX.init([0u8; 4096]);

        loop {
            net_stack.wait_config_up().await;
            net_stack.wait_link_up().await;

            if let Some(ipv4) = net_stack.config_v4() {
                esp_println::println!("connected, my IP: {:?}", ipv4.address);
                STATE.lock().await.ip_address = Some(ipv4.address.address());
            }

            let server = picoserve::Server::new(&router, &config, http_buffer);

            embassy_futures::select::select(
                net_stack.wait_link_down(),
                server.listen_and_serve(0u8, net_stack, 80, rx_buffer, tx_buffer),
            )
            .await;

            esp_println::println!("lost connection?");
            STATE.lock().await.ip_address = None;
            Timer::after_millis(500).await
        }
    };

    let sensors_task = async move {
        loop {
            led_0.toggle();

            let mq_value = adc.read_oneshot(&mut adc_pin).await as f32 / 4095.0;
            let bme_value = bme.measure(&mut Delay);
            let time = Instant::now().as_millis();

            {
                STATE.lock().await.measurement = if let Ok(bme_value) = bme_value {
                    Measurement {
                        temperature: bme_value.temperature,
                        pressure: bme_value.pressure / 100.0,
                        humidity: bme_value.humidity,
                        air_quality: mq_value,
                        time_millis: time,
                    }
                } else {
                    Measurement {
                        temperature: 0.0,
                        pressure: 0.0,
                        humidity: 0.0,
                        air_quality: mq_value,
                        time_millis: time,
                    }
                };
            }

            if let Ok(display) = display.as_mut() {
                let measurement = STATE.lock().await.measurement.clone();
                let addr = STATE.lock().await.ip_address.clone();
                let mut info_text = format!(
                    "Measurement:\n temperature: {:.2} C\n pressure: {:.0} hPa\n humidity: {:.2} %\n air quality: {:.3}\n\nip address:\n ",
                    measurement.temperature,
                    measurement.pressure,
                    measurement.humidity,
                    measurement.air_quality,
                );

                if let Some(addr) = addr {
                    info_text.push_str(&format!("{:?}", addr));
                } else {
                    info_text.push_str("not assigned");
                }

                display.clear_buffer();
                display.draw_str(0, 0, &info_text);
                let _ = display.flush();
            }

            Timer::after_millis(500).await;
        }
    };

    embassy_futures::join::join(
        embassy_futures::join::join(w5500_runner_task, net_runner_task),
        embassy_futures::join::join(
            embassy_futures::join::join(sensors_task, http_server_task),
            net_dhcp_task,
        ),
    )
    .await;

    loop {}
}
