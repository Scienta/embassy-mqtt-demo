#![no_std]
#![no_main]
extern crate alloc;

use core::fmt::Write;
use core::ops::Sub;
use defmt::{error, info, println};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_mqtt_demo::colors::*;
use embassy_net::dns::DnsQueryType;
use embassy_net::{tcp::TcpSocket, Ipv4Address, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::rmt::{Channel, Rmt};
use esp_hal::{prelude::*, rng::Rng, time, timer::timg::TimerGroup, Blocking};
use esp_hal_smartled::{smartLedBuffer, SmartLedsAdapter};
use esp_wifi::{
    init,
    wifi::{
        ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiStaDevice,
        WifiState,
    },
    EspWifiController,
};
use heapless::String;
use rgb::RGB8;
use rust_mqtt::packet::v5::publish_packet::QualityOfService;
use rust_mqtt::{
    client::{client::MqttClient, client_config::ClientConfig},
    packet::v5::reason_codes::ReasonCode,
    utils::rng_generator::CountingRng,
};
use smart_leds_trait::SmartLedsWrite;
use time::now;

// When you are okay with using a nightly compiler it's better to use https://docs.rs/static_cell/2.1.0/static_cell/macro.make_static.html
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const MQTT_CLIENT_ID: &str = env!("MQTT_CLIENT_ID");

#[main]
async fn main(spawner: Spawner) -> ! {
    // esp_println::logger::init_logger_from_env();
    let peripherals = esp_hal::init({
        let mut config = esp_hal::Config::default();
        config.cpu_clock = CpuClock::max();
        config
    });

    esp_alloc::heap_allocator!(72 * 1024);

    println!("println println");
    info!("info info");

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let mut rng = Rng::new(peripherals.RNG);

    let init = &*mk_static!(
        EspWifiController<'static>,
        init(timg0.timer0, rng, peripherals.RADIO_CLK,).unwrap()
    );

    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(init, wifi, WifiStaDevice).unwrap();

    let led_pin = peripherals.GPIO8;
    let freq = 80.MHz();
    let rmt = Rmt::new(peripherals.RMT, freq).unwrap();
    let rmt_buffer = smartLedBuffer!(1);
    let led = SmartLedsAdapter::new(rmt.channel0, led_pin, rmt_buffer);

    use esp_hal::timer::systimer::{SystemTimer, Target};
    let systimer = SystemTimer::new(peripherals.SYSTIMER).split::<Target>();
    esp_hal_embassy::init(systimer.alarm0);

    let config = embassy_net::Config::dhcpv4(Default::default());

    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // Init network stack
    let stack = &*mk_static!(
        Stack<WifiDevice<'_, WifiStaDevice>>,
        Stack::new(
            wifi_interface,
            config,
            mk_static!(StackResources<3>, StackResources::<3>::new()),
            seed
        )
    );

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(stack)).ok();
    spawner.spawn(led_task(led)).ok();

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    let start = now();
    info!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            info!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }
    let ip_duration = now().sub(start);
    info!("Got IP in {} ms", ip_duration.to_millis());

    // http_loop(stack).await
    mqtt_loop(stack).await
}

#[allow(dead_code)]
async fn http_loop(stack: &Stack<WifiDevice<'_, WifiStaDevice>>) -> ! {
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    loop {
        Timer::after(Duration::from_millis(1_000)).await;

        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);

        socket.set_timeout(Some(Duration::from_secs(10)));

        let remote_endpoint = (Ipv4Address::new(142, 250, 185, 115), 80);
        info!("connecting...");
        let r = socket.connect(remote_endpoint).await;
        if let Err(_e) = r {
            // println!("connect error: {:?}", e);
            info!("connect error");
            continue;
        }
        info!("connected!");
        let mut buf = [0; 1024];
        loop {
            use embedded_io_async::Write;
            let r = socket
                .write_all(b"GET / HTTP/1.0\r\nHost: www.mobile-j.de\r\n\r\n")
                .await;
            if let Err(_e) = r {
                // println!("write error: {:?}", e);
                info!("write error:");
                break;
            }
            let n = match socket.read(&mut buf).await {
                Ok(0) => {
                    info!("read EOF");
                    break;
                }
                Ok(n) => n,
                Err(_e) => {
                    // println!("read error: {:?}", e);
                    info!("read error:");
                    break;
                }
            };
            info!("{}", core::str::from_utf8(&buf[..n]).unwrap());
        }
        Timer::after(Duration::from_millis(3000)).await;
    }
}

async fn mqtt_loop(stack: &Stack<WifiDevice<'_, WifiStaDevice>>) -> ! {
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    let mut count = 0usize;

    loop {
        Timer::after(Duration::from_millis(1_000)).await;

        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);

        socket.set_timeout(Some(Duration::from_secs(10)));

        let address = match stack
            .dns_query("broker.hivemq.com", DnsQueryType::A)
            .await
            .map(|a| a[0])
        {
            Ok(address) => address,
            Err(_e) => {
                // error!("DNS lookup error: {e:?}");
                error!("DNS lookup error");
                continue;
            }
        };

        let remote_endpoint = (address, 1883);
        info!("connecting...");
        let connection = socket.connect(remote_endpoint).await;
        if let Err(_e) = connection {
            // error!("connect error: {:?}", e);
            error!("connect error:");
            continue;
        }
        info!("connected!");

        let mut config = ClientConfig::new(
            rust_mqtt::client::client_config::MqttVersion::MQTTv5,
            CountingRng(20000),
        );
        config.add_max_subscribe_qos(QualityOfService::QoS1);
        config.add_client_id(MQTT_CLIENT_ID);
        config.max_packet_size = 100;
        let mut recv_buffer = [0; 80];
        let mut write_buffer = [0; 80];

        let mut client =
            MqttClient::<_, 5, _>::new(socket, &mut write_buffer, 80, &mut recv_buffer, 80, config);

        match client.connect_to_broker().await {
            Ok(()) => {}
            Err(mqtt_error) => match mqtt_error {
                ReasonCode::NetworkError => {
                    error!("MQTT Network Error");
                    continue;
                }
                _ => {
                    // error!("Other MQTT Error: {:?}", mqtt_error);
                    error!("Other MQTT Error:");
                    continue;
                }
            },
        }

        info!("connected!");

        let mut msg: String<32> = String::new();
        write!(msg, "{:.2}", count).expect("write! failed!");

        match client
            .send_message(
                "scienta/trygvis/1",
                msg.as_bytes(),
                QualityOfService::QoS1,
                true,
            )
            .await
        {
            Ok(()) => info!("temp updated"),
            Err(_) => info!("mqtt error"),
        }

        // let mut bmp = Bmp180::new(i2c0, sleep).await;
        // loop {
        //     bmp.measure().await;
        //     let temperature = bmp.get_temperature();
        //     info!("Current temperature: {}", temperature);
        //
        //     // Convert temperature into String
        //     let mut temperature_string: String<32> = String::new();
        //     write!(temperature_string, "{:.2}", temperature).expect("write! failed!");
        //
        //     match client
        //         .send_message(
        //             "temperature/1",
        //             temperature_string.as_bytes(),
        //             rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1,
        //             true,
        //         )
        //         .await
        //     {
        //         Ok(()) => {}
        //         Err(mqtt_error) => match mqtt_error {
        //             ReasonCode::NetworkError => {
        //                 error!("MQTT Network Error");
        //                 continue;
        //             }
        //             _ => {
        //                 error!("Other MQTT Error: {:?}", mqtt_error);
        //                 continue;
        //             }
        //         },
        //     }
        //     Timer::after(Duration::from_millis(3000)).await;
        // }

        count += 1;
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    info!("start connection task");
    // println!("Device capabilities: {:?}", controller.capabilities());
    loop {
        if matches!(esp_wifi::wifi::wifi_state(), WifiState::StaConnected) {
            // wait until we're no longer connected
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            Timer::after(Duration::from_millis(5000)).await
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.try_into().unwrap(),
                password: PASSWORD.try_into().unwrap(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            info!("Starting wifi");
            controller.start_async().await.unwrap();
            info!("Wifi started!");
        }
        info!("About to connect...");

        match controller.connect_async().await {
            Ok(_) => info!("Wifi connected!"),
            Err(e) => {
                info!("Failed to connect to wifi: {:?}", e);
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>) {
    stack.run().await
}

#[embassy_executor::task]
async fn led_task(mut led: SmartLedsAdapter<Channel<Blocking, 0>, 25>) {
    const COLORS: [RGB8; 3] = [RED, BLUE, YELLOW];

    let mut idx = 0usize;

    loop {
        let data = [COLORS[idx]];
        idx = if idx == COLORS.len() - 1 { 0 } else { idx + 1 };
        let _ = led.write(data);
        Timer::after(Duration::from_secs(1)).await;
    }
}
