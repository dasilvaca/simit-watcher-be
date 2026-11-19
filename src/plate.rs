//! Colombian plate validation. Car `ABC123`, motorcycle `ABC12D`.

use crate::model::{PlateError, VehicleKind};

/// Uppercases and drops spaces and hyphens ("tto 08-g" -> "TTO08G").
pub fn normalize(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .flat_map(|c| c.to_uppercase())
        .collect()
}

/// Validates a normalized-or-raw plate for the given kind and returns the normalized plate.
pub fn validate(raw: &str, kind: VehicleKind) -> Result<String, PlateError> {
    let plate = normalize(raw);
    if plate.is_empty() {
        return Err(PlateError::Empty);
    }
    if plate.chars().count() != 6 {
        return Err(PlateError::WrongLength);
    }
    let c: Vec<char> = plate.chars().collect();
    let letters = c[..3].iter().all(|ch| ch.is_ascii_uppercase());
    let ok = match kind {
        VehicleKind::Car => letters && c[3..].iter().all(|ch| ch.is_ascii_digit()),
        VehicleKind::Moto => {
            letters && c[3].is_ascii_digit() && c[4].is_ascii_digit() && c[5].is_ascii_uppercase()
        }
    };
    if ok {
        Ok(plate)
    } else {
        Err(match kind {
            VehicleKind::Car => PlateError::WrongFormatCar,
            VehicleKind::Moto => PlateError::WrongFormatMoto,
        })
    }
}

/// Guesses the kind from a plate's shape, if it matches exactly one.
pub fn detect_kind(raw: &str) -> Option<VehicleKind> {
    if validate(raw, VehicleKind::Car).is_ok() {
        Some(VehicleKind::Car)
    } else if validate(raw, VehicleKind::Moto).is_ok() {
        Some(VehicleKind::Moto)
    } else {
        None
    }
}

/// Masks a plate for shareable logs: "TTO08G" -> "TTO***".
pub fn mask(plate: &str) -> String {
    let p = normalize(plate);
    let head: String = p.chars().take(3).collect();
    format!("{head}***")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn car_and_moto_formats() {
        assert_eq!(validate("abc123", VehicleKind::Car).unwrap(), "ABC123");
        assert_eq!(validate("tto 08-g", VehicleKind::Moto).unwrap(), "TTO08G");
        assert_eq!(
            validate("ABC12D", VehicleKind::Car),
            Err(PlateError::WrongFormatCar)
        );
        assert_eq!(
            validate("ABC123", VehicleKind::Moto),
            Err(PlateError::WrongFormatMoto)
        );
        assert_eq!(
            validate("AB123", VehicleKind::Car),
            Err(PlateError::WrongLength)
        );
        assert_eq!(validate("  ", VehicleKind::Car), Err(PlateError::Empty));
        assert_eq!(
            validate("ÁBC123", VehicleKind::Car),
            Err(PlateError::WrongFormatCar)
        );
    }

    #[test]
    fn detects_kind_and_masks() {
        assert_eq!(detect_kind("ABC123"), Some(VehicleKind::Car));
        assert_eq!(detect_kind("ABC12D"), Some(VehicleKind::Moto));
        assert_eq!(detect_kind("ABCDEF"), None);
        assert_eq!(mask("TTO08G"), "TTO***");
    }
}
