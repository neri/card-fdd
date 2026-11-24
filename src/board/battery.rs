//! Battery voltage: fed to G10 (ADC1) through a 1/2 voltage divider (100k + 100k on both v1.1 and Adv).

use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcConfig, AdcPin, Attenuation};
use esp_hal::peripherals::{ADC1, GPIO10};

use crate::board::Battery;

/// Number of samples averaged per measurement.
const SAMPLES: u32 = 16;

pub struct AdcBattery {
    adc: Adc<'static, ADC1<'static>, Blocking>,
    pin: AdcPin<GPIO10<'static>, ADC1<'static>, AdcCalCurve<ADC1<'static>>>,
}

impl AdcBattery {
    pub fn new(adc: ADC1<'static>, pin: GPIO10<'static>) -> Self {
        let mut config = AdcConfig::new();
        let pin = config.enable_pin_with_cal::<_, AdcCalCurve<ADC1<'static>>>(pin, Attenuation::_11dB);
        Self {
            adc: Adc::new(adc, config),
            pin,
        }
    }
}

impl Battery for AdcBattery {
    fn millivolts(&mut self) -> u16 {
        let sum: u32 = (0..SAMPLES)
            .map(|_| u32::from(self.adc.read_blocking(&mut self.pin)))
            .sum();
        // The calibrated value is the ADC pin voltage [mV]. Double it to undo the divider.
        (sum / SAMPLES * 2) as u16
    }
}
