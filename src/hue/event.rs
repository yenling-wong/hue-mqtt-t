use std::{collections::HashMap, sync::RwLock};

use color_eyre::Result;
use eyre::eyre;
use futures::TryStreamExt;
use palette::{FromColor, Hsv, Yxy};
use serde::Deserialize;

use crate::{
    mqtt_device::MqttDevice,
    protocols::{
        eventsource::PinnedEventSourceStream,
        mqtt::{publish_mqtt_device, MqttClient},
    },
    settings::Settings,
};

use super::{
    init_state::init_state_to_mqtt_devices,
    rest::{
        light::{ColorData, ColorTemperatureData, DimmingData, OnData},
        HueState,
    },
};

#[derive(Deserialize, Debug, Clone)]
struct ButtonData {
    last_event: String,
}

#[derive(Deserialize, Debug, Clone)]
struct ButtonUpdateData {
    id: String,
    button: ButtonData,
}

#[derive(Deserialize, Debug, Clone)]
struct LightUpdateData {
    id: String,
    on: Option<OnData>,
    dimming: Option<DimmingData>,
    color: Option<ColorData>,
    color_temperature: Option<ColorTemperatureData>,
}

#[derive(Deserialize, Debug, Clone)]
struct MotionData {
    motion: bool,
}

#[derive(Deserialize, Debug, Clone)]
struct MotionUpdateData {
    id: String,
    motion: MotionData,
}

#[derive(Deserialize, Debug, Clone)]
struct DevicePowerData {}

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UpdateData {
    Button(ButtonUpdateData),
    Light(LightUpdateData),
    Motion(MotionUpdateData),

    // Ignored updates
    Temperature,        // Temperature sensor update
    LightLevel,         // Light sensor update
    DevicePower,        // Battery level update
    GroupedLight,       // Light groups update
    ZigbeeConnectivity, // Connectivity issue update
}

#[derive(Deserialize, Debug, Clone)]
pub struct UpdateEvent {
    data: Vec<UpdateData>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HueEvent {
    Update(UpdateEvent),
}

/// Computes current device state from previous device state and an UpdateData
/// containing one field that's changed.
fn hue_event_data_to_mqtt_device(
    data: &UpdateData,
    mqtt_devices: &HashMap<String, MqttDevice>,
) -> Option<MqttDevice> {
    match data {
        UpdateData::Button(button) => {
            let sensor_value = match button.button.last_event.as_str() {
                "short_release" | "long_release" => Some(false),
                "initial_press" => Some(true),
                _ => None,
            };

            if let Some(sensor_value) = sensor_value {
                let mut mqtt_device = mqtt_devices.get(&button.id)?.clone();

                mqtt_device.sensor_value = Some(sensor_value.to_string());

                return Some(mqtt_device);
            }
        }
        UpdateData::Motion(motion) => {
            let mut mqtt_device = mqtt_devices.get(&motion.id)?.clone();

            mqtt_device.sensor_value = Some(motion.motion.motion.to_string());

            return Some(mqtt_device);
        }
        UpdateData::Light(light) => {
            let mut mqtt_device = mqtt_devices.get(&light.id)?.clone();

            if let Some(color) = &light.color {
                let mut hsv = Hsv::from_color(Yxy::new(color.xy.x, color.xy.y, 1.0));
                hsv.value = 1.0;

                mqtt_device.color = Some(hsv);
            }

            if let Some(ColorTemperatureData { mirek: Some(mirek) }) = light.color_temperature {
                let cct = 1_000_000.0 / mirek;
                mqtt_device.cct = Some(cct);
            }

            if let Some(on) = &light.on {
                mqtt_device.power = Some(on.on);
            }

            if let Some(dimming) = &light.dimming {
                mqtt_device.brightness = Some(dimming.brightness / 100.0)
            }

            return Some(mqtt_device);
        }

        _ => {}
    };

    None
}

pub fn try_parse_hue_events(
    mqtt_devices: &RwLock<HashMap<String, MqttDevice>>,
    events: String,
) -> Result<Vec<MqttDevice>> {
    let serde_json_value: serde_json::Value = serde_json::from_str(&events)?;
    let result = serde_json::from_str::<Vec<HueEvent>>(&events);

    match result {
        Ok(events) => {
            let update_data_vec: Vec<UpdateData> = events
                .into_iter()
                .flat_map(|event| {
                    let HueEvent::Update(event) = event;

                    event.data
                })
                .collect();

            // We only want light_updates to contain the the result of applying
            // all incoming device state updates. This way we don't spam mqtt
            // with the Hue bridge's "halfway" state updates.
            //
            // Hue's eventsource API splits a light update into multiple
            // "UpdateData" chunks, with each chunk containing the change to a
            // single field. So if we send one HTTP request simultaneously
            // changing a light's power state, color and brightness, you will
            // get back three events, one for each field.
            let mut light_updates: HashMap<String, MqttDevice> = HashMap::new();
            for data in update_data_vec
                .iter()
                .filter(|data| matches!(data, UpdateData::Light(_)))
            {
                let mut mqtt_devices = mqtt_devices.write().unwrap();
                let mqtt_device = hue_event_data_to_mqtt_device(data, &mqtt_devices);

                if let Some(mqtt_device) = &mqtt_device {
                    // Store device state as computed from previous state and
                    // the event being handled
                    mqtt_devices.insert(mqtt_device.id.clone(), mqtt_device.clone());
                    light_updates.insert(mqtt_device.id.clone(), mqtt_device.clone());
                }
            }

            // We want sensor_updates to contain all intermediate device states.
            // The reason is that we want to inform mqtt clients of situations
            // where sensor state rapidly changes between two values (such as
            // when pressing a button in rapid succession).
            //
            // For example, if a switch is pressed four times with just under
            // 1s between presses, due to the Hue bridge's debouncing of the
            // eventsource API events, we get sent the following sequence of
            // messages of the switch pressed state:
            //
            // [true], [false, true, false], [true, false], [true, false]
            //
            // If we were to only forward the trailing value of each message,
            // mqtt would only see [true, false, false, false].

            let sensor_updates: Vec<MqttDevice> = update_data_vec
                .iter()
                .filter(|data| matches!(data, UpdateData::Button(_) | UpdateData::Motion(_)))
                .filter_map(|data| {
                    let mut mqtt_devices = mqtt_devices.write().unwrap();
                    let mqtt_device = hue_event_data_to_mqtt_device(data, &mqtt_devices);

                    if let Some(mqtt_device) = &mqtt_device {
                        mqtt_devices.insert(mqtt_device.id.clone(), mqtt_device.clone());
                    }

                    mqtt_device
                })
                .collect();

            let updates = light_updates
                .into_values()
                .chain(sensor_updates.into_iter())
                .collect();

            Ok(updates)
        }
        Err(e) => {
            eprintln!(
                "Got unknown event:\n{}",
                serde_json::to_string_pretty(&serde_json_value)?
            );
            Err(eyre!(e))
        }
    }
}

pub fn start_eventsource_events_loop(
    mut eventsource_stream: PinnedEventSourceStream,
    settings: &Settings,
    mqtt_client: &MqttClient,
    init_state: &HueState,
) {
    let mqtt_client = mqtt_client.clone();
    let settings = settings.clone();
    let init_state = init_state.clone();

    // Somewhat annoyingly, the Hue eventsource endpoint returns changed fields
    // of a device in individual chunks. We need to persist these changes across
    // incoming events to be able to piece together current device state.
    let mqtt_devices = RwLock::new(init_state_to_mqtt_devices(&init_state));

    tokio::spawn(async move {
        while let Ok(Some(e)) = eventsource_stream.try_next().await {
            if let eventsource_client::SSE::Event(e) = e {
                let result = try_parse_hue_events(&mqtt_devices, e.data);
                match result {
                    Ok(mqtt_devices) => {
                        for mqtt_device in mqtt_devices {
                            let result =
                                publish_mqtt_device(&mqtt_client, &settings, &mqtt_device).await;

                            if let Err(e) = result {
                                eprintln!("{:?}", e);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("{:?}", e);
                    }
                }
            }
        }
    });
}
