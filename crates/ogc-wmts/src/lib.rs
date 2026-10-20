use std::collections::HashMap;

use thiserror::Error;

pub const WMTS_VERSION: &str = "1.0.0";
pub const MATRIX_SET: &str = "WebMercatorQuad";
pub const TILE_SIZE: u32 = 256;
pub const MAX_ZOOM: u8 = 22;
pub const WEB_MERCATOR_HALF_WORLD: f64 = 20_037_508.342_789_244;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    GetCapabilities,
    GetTile,
}

#[derive(Debug, Clone)]
pub struct WmtsRequest {
    pub operation: Operation,
    pub parameters: HashMap<String, String>,
}

#[derive(Debug, Error)]
pub enum WmtsError {
    #[error("missing required parameter: {0}")]
    MissingParameter(&'static str),
    #[error("unsupported operation: {0}")]
    OperationNotSupported(String),
    #[error("invalid parameter {0}: {1}")]
    InvalidParameter(&'static str, String),
}

impl WmtsRequest {
    pub fn from_query(parameters: HashMap<String, String>) -> Result<Self, WmtsError> {
        let parameters = parameters
            .into_iter()
            .map(|(key, value)| (key.to_ascii_uppercase(), value))
            .collect::<HashMap<_, _>>();
        let request = parameters
            .get("REQUEST")
            .ok_or(WmtsError::MissingParameter("REQUEST"))?;
        let operation = match request.to_ascii_lowercase().as_str() {
            "getcapabilities" => Operation::GetCapabilities,
            "gettile" => Operation::GetTile,
            _ => return Err(WmtsError::OperationNotSupported(request.clone())),
        };
        Ok(Self {
            operation,
            parameters,
        })
    }

    pub fn tile(&self) -> Result<TileRequest, WmtsError> {
        fn required<'a>(
            values: &'a HashMap<String, String>,
            key: &'static str,
        ) -> Result<&'a str, WmtsError> {
            values
                .get(key)
                .map(String::as_str)
                .ok_or(WmtsError::MissingParameter(key))
        }
        let matrix_set = required(&self.parameters, "TILEMATRIXSET")?;
        if matrix_set != MATRIX_SET {
            return Err(WmtsError::InvalidParameter(
                "TILEMATRIXSET",
                matrix_set.into(),
            ));
        }
        let zoom = required(&self.parameters, "TILEMATRIX")?
            .parse::<u8>()
            .map_err(|_| WmtsError::InvalidParameter("TILEMATRIX", "not an integer".into()))?;
        let row = required(&self.parameters, "TILEROW")?
            .parse::<u32>()
            .map_err(|_| WmtsError::InvalidParameter("TILEROW", "not an integer".into()))?;
        let col = required(&self.parameters, "TILECOL")?
            .parse::<u32>()
            .map_err(|_| WmtsError::InvalidParameter("TILECOL", "not an integer".into()))?;
        let matrix_size = matrix_size(zoom)?;
        if row >= matrix_size || col >= matrix_size {
            return Err(WmtsError::InvalidParameter(
                "TILEROW/TILECOL",
                "outside tile matrix".into(),
            ));
        }
        let pixel_ratio = self
            .parameters
            .get("PIXELRATIO")
            .map(String::as_str)
            .unwrap_or("1")
            .parse::<u8>()
            .map_err(|_| WmtsError::InvalidParameter("PIXELRATIO", "not an integer".into()))?;
        if !matches!(pixel_ratio, 1 | 2) {
            return Err(WmtsError::InvalidParameter(
                "PIXELRATIO",
                "must be 1 or 2".into(),
            ));
        }
        Ok(TileRequest {
            layer: required(&self.parameters, "LAYER")?.into(),
            style: self
                .parameters
                .get("STYLE")
                .cloned()
                .unwrap_or_else(|| "default".into()),
            zoom,
            row,
            col,
            pixel_ratio,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileRequest {
    pub layer: String,
    pub style: String,
    pub zoom: u8,
    pub row: u32,
    pub col: u32,
    /// Non-standard HiDPI extension. A value of 2 returns a 512px image for a
    /// logical 256px WMTS tile.
    pub pixel_ratio: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileBounds {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

pub fn matrix_size(zoom: u8) -> Result<u32, WmtsError> {
    if zoom > MAX_ZOOM {
        return Err(WmtsError::InvalidParameter(
            "TILEMATRIX",
            format!("maximum zoom is {MAX_ZOOM}"),
        ));
    }
    Ok(1_u32 << zoom)
}

pub fn tile_bounds(zoom: u8, row: u32, col: u32) -> Result<TileBounds, WmtsError> {
    let size = matrix_size(zoom)?;
    if row >= size || col >= size {
        return Err(WmtsError::InvalidParameter(
            "TILEROW/TILECOL",
            "outside tile matrix".into(),
        ));
    }
    let span = WEB_MERCATOR_HALF_WORLD * 2.0 / f64::from(size);
    Ok(TileBounds {
        min_x: -WEB_MERCATOR_HALF_WORLD + f64::from(col) * span,
        max_x: -WEB_MERCATOR_HALF_WORLD + f64::from(col + 1) * span,
        max_y: WEB_MERCATOR_HALF_WORLD - f64::from(row) * span,
        min_y: WEB_MERCATOR_HALF_WORLD - f64::from(row + 1) * span,
    })
}

#[derive(Debug, Clone)]
pub struct CapabilitiesLayer {
    pub identifier: String,
    pub title: String,
}

pub fn capabilities_xml(service_url: &str, layers: &[CapabilitiesLayer]) -> String {
    let url = xml_escape(service_url);
    let mut layer_xml = String::new();
    for layer in layers {
        layer_xml.push_str(&format!("<Layer><ows:Title>{}</ows:Title><ows:Identifier>{}</ows:Identifier><Style isDefault=\"true\"><ows:Identifier>default</ows:Identifier></Style><Format>image/png</Format><TileMatrixSetLink><TileMatrixSet>{MATRIX_SET}</TileMatrixSet></TileMatrixSetLink></Layer>", xml_escape(&layer.title), xml_escape(&layer.identifier)));
    }
    let mut matrices = String::new();
    for zoom in 0..=MAX_ZOOM {
        let size = 1_u32 << zoom;
        let resolution = WEB_MERCATOR_HALF_WORLD * 2.0 / (f64::from(TILE_SIZE) * f64::from(size));
        let scale = resolution / 0.00028;
        matrices.push_str(&format!("<TileMatrix><ows:Identifier>{zoom}</ows:Identifier><ScaleDenominator>{scale}</ScaleDenominator><TopLeftCorner>-20037508.342789244 20037508.342789244</TopLeftCorner><TileWidth>{TILE_SIZE}</TileWidth><TileHeight>{TILE_SIZE}</TileHeight><MatrixWidth>{size}</MatrixWidth><MatrixHeight>{size}</MatrixHeight></TileMatrix>"));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Capabilities xmlns="http://www.opengis.net/wmts/1.0" xmlns:ows="http://www.opengis.net/ows/1.1" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://www.opengis.net/wmts/1.0 https://schemas.opengis.net/wmts/1.0/wmtsGetCapabilities_response.xsd" version="1.0.0"><ows:ServiceIdentification><ows:Title>Cangling Map WMTS</ows:Title><ows:ServiceType>OGC WMTS</ows:ServiceType><ows:ServiceTypeVersion>1.0.0</ows:ServiceTypeVersion></ows:ServiceIdentification><ows:OperationsMetadata><ows:Operation name="GetCapabilities"><ows:DCP><ows:HTTP><ows:Get xlink:href="{url}"/></ows:HTTP></ows:DCP></ows:Operation><ows:Operation name="GetTile"><ows:DCP><ows:HTTP><ows:Get xlink:href="{url}"/></ows:HTTP></ows:DCP></ows:Operation></ows:OperationsMetadata><Contents>{layer_xml}<TileMatrixSet><ows:Identifier>{MATRIX_SET}</ows:Identifier><ows:SupportedCRS>urn:ogc:def:crs:EPSG::3857</ows:SupportedCRS><WellKnownScaleSet>urn:ogc:def:wkss:OGC:1.0:GoogleMapsCompatible</WellKnownScaleSet>{matrices}</TileMatrixSet></Contents><ServiceMetadataURL xlink:href="{url}?SERVICE=WMTS&amp;REQUEST=GetCapabilities&amp;VERSION=1.0.0"/></Capabilities>"#
    )
}

pub fn exception_xml(code: &str, locator: &str, message: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><ows:ExceptionReport xmlns:ows="http://www.opengis.net/ows/1.1" version="1.0.0"><ows:Exception exceptionCode="{}" locator="{}"><ows:ExceptionText>{}</ows:ExceptionText></ows:Exception></ows:ExceptionReport>"#,
        xml_escape(code),
        xml_escape(locator),
        xml_escape(message)
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_tile_has_web_mercator_bounds() {
        let bounds = tile_bounds(0, 0, 0).unwrap();
        assert_eq!(bounds.min_x, -WEB_MERCATOR_HALF_WORLD);
        assert_eq!(bounds.max_y, WEB_MERCATOR_HALF_WORLD);
        assert_eq!(bounds.max_x, WEB_MERCATOR_HALF_WORLD);
        assert_eq!(bounds.min_y, -WEB_MERCATOR_HALF_WORLD);
    }

    #[test]
    fn north_west_quarter_is_correct() {
        let bounds = tile_bounds(1, 0, 0).unwrap();
        assert_eq!(bounds.min_x, -WEB_MERCATOR_HALF_WORLD);
        assert_eq!(bounds.min_y, 0.0);
        assert_eq!(bounds.max_x, 0.0);
        assert_eq!(bounds.max_y, WEB_MERCATOR_HALF_WORLD);
    }

    #[test]
    fn parses_supported_hidpi_pixel_ratio() {
        let request = WmtsRequest::from_query(HashMap::from([
            ("REQUEST".into(), "GetTile".into()),
            ("TILEMATRIXSET".into(), MATRIX_SET.into()),
            ("TILEMATRIX".into(), "3".into()),
            ("TILEROW".into(), "2".into()),
            ("TILECOL".into(), "4".into()),
            ("LAYER".into(), "roads".into()),
            ("PIXELRATIO".into(), "2".into()),
        ]))
        .unwrap();
        assert_eq!(request.tile().unwrap().pixel_ratio, 2);
    }
}
