use map_core::Rgba;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Style {
    pub name: String,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub filter: Option<Filter>,
    pub minimum_scale_denominator: Option<f64>,
    pub maximum_scale_denominator: Option<f64>,
    pub symbolizer: PolygonSymbolizer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolygonSymbolizer {
    pub fill: Option<ColorExpression>,
    #[serde(default)]
    pub fill_image: Option<String>,
    #[serde(default = "default_image_scale")]
    pub fill_image_scale: f32,
    pub fill_opacity: f32,
    pub stroke: Option<ColorExpression>,
    pub stroke_opacity: f32,
    pub stroke_width: f32,
    #[serde(default)]
    pub stroke_dasharray: Vec<f32>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default = "default_font_family")]
    pub font_family: String,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    #[serde(default)]
    pub font_italic: bool,
    #[serde(default)]
    pub font_bold: bool,
    #[serde(default = "default_font_fill")]
    pub font_fill: Rgba,
    #[serde(default = "default_halo_color")]
    pub halo_color: Rgba,
    #[serde(default = "default_halo_radius")]
    pub halo_radius: f32,
    #[serde(default)]
    pub label_spacing: f32,
    #[serde(default)]
    pub label_min_area: f32,
    #[serde(default)]
    pub label_repeat_distance: f32,
    #[serde(default = "default_label_repeat_count")]
    pub label_repeat_count: u32,
}

fn default_label_repeat_count() -> u32 {
    1
}

fn default_image_scale() -> f32 {
    1.0
}

fn default_font_family() -> String {
    "Noto Sans CJK SC".into()
}
fn default_font_size() -> f32 {
    13.0
}
fn default_font_fill() -> Rgba {
    Rgba {
        red: 0x24,
        green: 0x33,
        blue: 0x2c,
        alpha: 255,
    }
}
fn default_halo_color() -> Rgba {
    Rgba {
        red: 255,
        green: 255,
        blue: 255,
        alpha: 255,
    }
}
fn default_halo_radius() -> f32 {
    2.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ColorExpression {
    Literal(Rgba),
    Property { property: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Filter {
    Comparison {
        property: String,
        operator: ComparisonOperator,
        value: Literal,
    },
    And {
        filters: Vec<Filter>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonOperator {
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Literal {
    String(String),
    Number(f64),
    Boolean(bool),
}

#[derive(Debug, Error, PartialEq)]
pub enum StyleError {
    #[error("style not found: {0}")]
    NotFound(String),
    #[error("GeoCSS error at line {line}, column {column}: {message}")]
    GeoCss {
        line: usize,
        column: usize,
        message: String,
    },
    #[error("invalid style: {0}")]
    Invalid(String),
}

pub trait StyleProvider: Send + Sync {
    fn resolve(&self, layer: &str, style: Option<&str>) -> Result<Style, StyleError>;
}

pub fn compile_geocss(name: impl Into<String>, source: &str) -> Result<Style, StyleError> {
    let clean = remove_comments(source)?;
    let mut rules = Vec::new();
    let mut cursor = 0;
    while let Some(relative_open) = clean[cursor..].find('{') {
        let open = cursor + relative_open;
        let selector = clean[cursor..open].trim();
        if selector.is_empty() {
            return Err(error_at(source, open, "missing rule selector"));
        }
        let close = clean[open + 1..]
            .find('}')
            .map(|n| open + 1 + n)
            .ok_or_else(|| error_at(source, open, "missing closing `}`"))?;
        if clean[open + 1..close].contains('{') {
            return Err(error_at(source, open + 1, "nested rules are not supported"));
        }
        rules.push(parse_rule(
            source,
            selector,
            &clean[open + 1..close],
            cursor,
        )?);
        cursor = close + 1;
    }
    if !clean[cursor..].trim().is_empty() {
        return Err(error_at(source, cursor, "expected a GeoCSS rule"));
    }
    if rules.is_empty() {
        return Err(error_at(source, 0, "style must contain at least one rule"));
    }
    Ok(Style {
        name: name.into(),
        rules,
    })
}

fn parse_rule(
    source: &str,
    selector: &str,
    declarations: &str,
    offset: usize,
) -> Result<Rule, StyleError> {
    let (filter, minimum_scale, maximum_scale) = parse_selector(source, selector, offset)?;
    let mut symbolizer = PolygonSymbolizer {
        fill: None,
        fill_image: None,
        fill_image_scale: 1.0,
        fill_opacity: 1.0,
        stroke: None,
        stroke_opacity: 1.0,
        stroke_width: 1.0,
        stroke_dasharray: Vec::new(),
        label: None,
        font_family: default_font_family(),
        font_size: default_font_size(),
        font_italic: false,
        font_bold: false,
        font_fill: default_font_fill(),
        halo_color: default_halo_color(),
        halo_radius: default_halo_radius(),
        label_spacing: 0.0,
        label_min_area: 0.0,
        label_repeat_distance: 0.0,
        label_repeat_count: 1,
    };
    for declaration in declarations
        .split(';')
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let (property, value) = declaration
            .split_once(':')
            .ok_or_else(|| error_at(source, offset, "declaration must contain `:`"))?;
        let (property, value) = (property.trim(), value.trim());
        match property {
            "fill" => symbolizer.fill = Some(parse_color_expression(source, value, offset)?),
            "fill-image" => symbolizer.fill_image = Some(parse_fill_image(source, value, offset)?),
            "fill-image-scale" => {
                symbolizer.fill_image_scale =
                    parse_positive_number(source, value, offset, "fill-image-scale")?
            }
            "fill-opacity" => symbolizer.fill_opacity = parse_opacity(source, value, offset)?,
            "stroke" => symbolizer.stroke = Some(parse_color_expression(source, value, offset)?),
            "stroke-opacity" => symbolizer.stroke_opacity = parse_opacity(source, value, offset)?,
            "stroke-width" => symbolizer.stroke_width = parse_width(source, value, offset)?,
            "stroke-dasharray" => {
                let values = value
                    .split(|character: char| character == ',' || character.is_whitespace())
                    .filter(|part| !part.is_empty())
                    .map(|part| {
                        parse_positive_number(
                            source,
                            part.trim_end_matches("px"),
                            offset,
                            "stroke-dasharray",
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if values.len() < 2 || values.len() % 2 != 0 {
                    return Err(error_at(
                        source,
                        offset,
                        "stroke-dasharray requires an even number of positive lengths",
                    ));
                }
                symbolizer.stroke_dasharray = values;
            }
            "label" => symbolizer.label = Some(parse_label_property(source, value, offset)?),
            "font-family" => symbolizer.font_family = value.trim_matches(['\'', '"']).to_owned(),
            "font-size" => {
                symbolizer.font_size = parse_positive_number(
                    source,
                    value.trim_end_matches("px"),
                    offset,
                    "font-size",
                )?
            }
            "font-style" => {
                symbolizer.font_italic = match value.to_ascii_lowercase().as_str() {
                    "normal" => false,
                    "italic" | "oblique" => true,
                    _ => {
                        return Err(error_at(
                            source,
                            offset,
                            "font-style must be normal, italic or oblique",
                        ))
                    }
                }
            }
            "font-weight" => {
                symbolizer.font_bold = match value.to_ascii_lowercase().as_str() {
                    "normal" | "400" => false,
                    "bold" | "600" | "700" => true,
                    _ => {
                        return Err(error_at(
                            source,
                            offset,
                            "font-weight must be normal, bold, 400, 600 or 700",
                        ))
                    }
                }
            }
            "font-fill" => symbolizer.font_fill = parse_color(source, value, offset)?,
            "halo-color" => symbolizer.halo_color = parse_color(source, value, offset)?,
            "halo-radius" => {
                symbolizer.halo_radius = parse_positive_number(
                    source,
                    value.trim_end_matches("px"),
                    offset,
                    "halo-radius",
                )?
            }
            "label-spacing" => symbolizer.label_spacing = parse_width(source, value, offset)?,
            "label-min-area" => symbolizer.label_min_area = parse_width(source, value, offset)?,
            "label-repeat-distance" => {
                symbolizer.label_repeat_distance = parse_width(source, value, offset)?
            }
            "label-repeat-count" => {
                let count = value.parse::<u32>().map_err(|_| {
                    error_at(
                        source,
                        offset,
                        "label-repeat-count must be a positive integer",
                    )
                })?;
                if count == 0 || count > 32 {
                    return Err(error_at(
                        source,
                        offset,
                        "label-repeat-count must be between 1 and 32",
                    ));
                }
                symbolizer.label_repeat_count = count;
            }
            _ => {
                return Err(error_at(
                    source,
                    offset,
                    &format!("unsupported property `{property}`"),
                ))
            }
        }
    }
    if symbolizer.fill.is_none()
        && symbolizer.fill_image.is_none()
        && symbolizer.stroke.is_none()
        && symbolizer.label.is_none()
    {
        return Err(error_at(
            source,
            offset,
            "polygon rule requires `fill` or `stroke`",
        ));
    }
    Ok(Rule {
        filter,
        minimum_scale_denominator: minimum_scale,
        maximum_scale_denominator: maximum_scale,
        symbolizer,
    })
}

fn parse_label_property(source: &str, value: &str, offset: usize) -> Result<String, StyleError> {
    let property = value
        .trim()
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .ok_or_else(|| {
            error_at(
                source,
                offset,
                "label must reference a field such as `[name]`",
            )
        })?;
    if property.trim().is_empty() {
        Err(error_at(source, offset, "label field cannot be empty"))
    } else {
        Ok(property.trim().to_owned())
    }
}

fn parse_fill_image(source: &str, value: &str, offset: usize) -> Result<String, StyleError> {
    let value = value.trim();
    let value = value
        .strip_prefix("url(")
        .and_then(|value| value.strip_suffix(')'))
        .unwrap_or(value)
        .trim()
        .trim_matches(['\'', '"']);
    if value.is_empty() {
        return Err(error_at(source, offset, "fill-image path cannot be empty"));
    }
    if value.contains("://") || value.starts_with("data:") {
        return Err(error_at(
            source,
            offset,
            "fill-image only supports local files",
        ));
    }
    let path = std::path::Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(error_at(
            source,
            offset,
            "fill-image must be relative to the style asset directory",
        ));
    }
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    if !extension.eq_ignore_ascii_case("png") && !extension.eq_ignore_ascii_case("webp") {
        return Err(error_at(
            source,
            offset,
            "fill-image must be a PNG or WebP file",
        ));
    }
    Ok(value.to_owned())
}

fn parse_positive_number(
    source: &str,
    value: &str,
    offset: usize,
    property: &str,
) -> Result<f32, StyleError> {
    let value = value
        .parse::<f32>()
        .map_err(|_| error_at(source, offset, &format!("{property} must be a number")))?;
    if value.is_finite() && value > 0.0 {
        Ok(value)
    } else {
        Err(error_at(
            source,
            offset,
            &format!("{property} must be greater than zero"),
        ))
    }
}

fn parse_selector(
    source: &str,
    selector: &str,
    offset: usize,
) -> Result<(Option<Filter>, Option<f64>, Option<f64>), StyleError> {
    let mut rest = selector.trim();
    let mut filters = Vec::new();
    let (mut minimum, mut maximum) = (None, None);
    if let Some(value) = rest.strip_prefix('*') {
        rest = value.trim();
    }
    while !rest.is_empty() {
        if !rest.starts_with('[') {
            return Err(error_at(
                source,
                offset,
                "selector must be `*` or `[condition]`",
            ));
        }
        let close = rest
            .find(']')
            .ok_or_else(|| error_at(source, offset, "missing closing `]`"))?;
        let condition = rest[1..close].trim();
        if condition.starts_with("@scale") {
            parse_scale(source, condition, offset, &mut minimum, &mut maximum)?;
        } else {
            filters.push(parse_filter(source, condition, offset)?);
        }
        rest = rest[close + 1..].trim();
    }
    let filter = match filters.len() {
        0 => None,
        1 => filters.pop(),
        _ => Some(Filter::And { filters }),
    };
    Ok((filter, minimum, maximum))
}

fn parse_scale(
    source: &str,
    condition: &str,
    offset: usize,
    minimum: &mut Option<f64>,
    maximum: &mut Option<f64>,
) -> Result<(), StyleError> {
    let expression = condition.trim_start_matches("@scale").trim();
    let (operator, value) = split_operator(expression)
        .ok_or_else(|| error_at(source, offset, "invalid @scale expression"))?;
    let value = value
        .trim()
        .parse::<f64>()
        .map_err(|_| error_at(source, offset, "scale denominator must be a number"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(error_at(
            source,
            offset,
            "scale denominator must be non-negative",
        ));
    }
    match operator {
        "<" | "<=" => *maximum = Some(maximum.map_or(value, |v| v.min(value))),
        ">" | ">=" => *minimum = Some(minimum.map_or(value, |v| v.max(value))),
        _ => {
            return Err(error_at(
                source,
                offset,
                "@scale supports only <, <=, > or >=",
            ))
        }
    }
    Ok(())
}

fn parse_filter(source: &str, expression: &str, offset: usize) -> Result<Filter, StyleError> {
    let (operator_text, right) = split_operator(expression)
        .ok_or_else(|| error_at(source, offset, "invalid attribute filter"))?;
    let index = expression.find(operator_text).unwrap_or(0);
    let property = expression[..index].trim();
    if property.is_empty()
        || !property
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(error_at(source, offset, "invalid property name"));
    }
    let operator = match operator_text {
        "=" => ComparisonOperator::Equal,
        "!=" | "<>" => ComparisonOperator::NotEqual,
        "<" => ComparisonOperator::LessThan,
        "<=" => ComparisonOperator::LessThanOrEqual,
        ">" => ComparisonOperator::GreaterThan,
        ">=" => ComparisonOperator::GreaterThanOrEqual,
        _ => return Err(error_at(source, offset, "unsupported filter operator")),
    };
    Ok(Filter::Comparison {
        property: property.to_owned(),
        operator,
        value: parse_literal(source, right.trim(), offset)?,
    })
}

fn split_operator(value: &str) -> Option<(&'static str, &str)> {
    for operator in ["<=", ">=", "!=", "<>", "=", "<", ">"] {
        if let Some(index) = value.find(operator) {
            return Some((operator, &value[index + operator.len()..]));
        }
    }
    None
}

fn parse_literal(source: &str, value: &str, offset: usize) -> Result<Literal, StyleError> {
    if value.len() >= 2
        && ((value.starts_with('\'') && value.ends_with('\''))
            || (value.starts_with('"') && value.ends_with('"')))
    {
        return Ok(Literal::String(value[1..value.len() - 1].to_owned()));
    }
    if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false") {
        return Ok(Literal::Boolean(value.eq_ignore_ascii_case("true")));
    }
    value.parse::<f64>().map(Literal::Number).map_err(|_| {
        error_at(
            source,
            offset,
            "literal must be quoted text, a number or boolean",
        )
    })
}

fn parse_color(source: &str, value: &str, offset: usize) -> Result<Rgba, StyleError> {
    let hex = value
        .strip_prefix('#')
        .ok_or_else(|| error_at(source, offset, "color must use #RGB or #RRGGBB notation"))?;
    let expanded;
    let hex = if hex.len() == 3 {
        expanded = hex.chars().flat_map(|c| [c, c]).collect::<String>();
        &expanded
    } else {
        hex
    };
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(error_at(
            source,
            offset,
            "color must use #RGB or #RRGGBB notation",
        ));
    }
    Ok(Rgba {
        red: u8::from_str_radix(&hex[0..2], 16).unwrap(),
        green: u8::from_str_radix(&hex[2..4], 16).unwrap(),
        blue: u8::from_str_radix(&hex[4..6], 16).unwrap(),
        alpha: 255,
    })
}

fn parse_color_expression(
    source: &str,
    value: &str,
    offset: usize,
) -> Result<ColorExpression, StyleError> {
    if let Some(property) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        let property = property.trim();
        if property.is_empty()
            || !property
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(error_at(
                source,
                offset,
                "color field must contain only letters, numbers or `_`",
            ));
        }
        return Ok(ColorExpression::Property {
            property: property.to_owned(),
        });
    }
    parse_color(source, value, offset).map(ColorExpression::Literal)
}

fn parse_opacity(source: &str, value: &str, offset: usize) -> Result<f32, StyleError> {
    let value = value
        .parse::<f32>()
        .map_err(|_| error_at(source, offset, "opacity must be a number between 0 and 1"))?;
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(error_at(source, offset, "opacity must be between 0 and 1"))
    }
}

fn parse_width(source: &str, value: &str, offset: usize) -> Result<f32, StyleError> {
    let value = value
        .strip_suffix("px")
        .unwrap_or(value)
        .trim()
        .parse::<f32>()
        .map_err(|_| error_at(source, offset, "stroke-width must be a number"))?;
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(error_at(
            source,
            offset,
            "stroke-width must be non-negative",
        ))
    }
}

fn remove_comments(source: &str) -> Result<String, StyleError> {
    let mut output = source.as_bytes().to_vec();
    let mut cursor = 0;
    while let Some(start_offset) = source[cursor..].find("/*") {
        let start = cursor + start_offset;
        let end = source[start + 2..]
            .find("*/")
            .map(|n| start + 2 + n)
            .ok_or_else(|| error_at(source, start, "unterminated comment"))?;
        for byte in &mut output[start..end + 2] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
        cursor = end + 2;
    }
    String::from_utf8(output).map_err(|error| StyleError::Invalid(error.to_string()))
}

fn error_at(source: &str, offset: usize, message: &str) -> StyleError {
    let prefix = &source[..offset.min(source.len())];
    let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix.chars().count() + 1, |(_, tail)| {
            tail.chars().count() + 1
        });
    StyleError::GeoCss {
        line,
        column,
        message: message.to_owned(),
    }
}

pub fn to_sld_1_1(style: &Style, layer_name: &str) -> Result<String, StyleError> {
    if style.rules.is_empty() {
        return Err(StyleError::Invalid("style has no rules".into()));
    }
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<sld:StyledLayerDescriptor version=\"1.1.0\" xmlns:sld=\"http://www.opengis.net/sld\" xmlns:se=\"http://www.opengis.net/se\" xmlns:ogc=\"http://www.opengis.net/ogc\">\n  <sld:NamedLayer>\n");
    xml.push_str(&format!("    <se:Name>{}</se:Name>\n    <sld:UserStyle>\n      <se:Name>{}</se:Name>\n      <se:FeatureTypeStyle>\n", escape_xml(layer_name), escape_xml(&style.name)));
    for rule in &style.rules {
        write_rule(&mut xml, rule);
    }
    xml.push_str("      </se:FeatureTypeStyle>\n    </sld:UserStyle>\n  </sld:NamedLayer>\n</sld:StyledLayerDescriptor>\n");
    Ok(xml)
}

fn write_rule(xml: &mut String, rule: &Rule) {
    xml.push_str("        <se:Rule>\n");
    if let Some(filter) = &rule.filter {
        xml.push_str("          <ogc:Filter>\n");
        write_filter(xml, filter, 12);
        xml.push_str("          </ogc:Filter>\n");
    }
    if let Some(value) = rule.minimum_scale_denominator {
        xml.push_str(&format!(
            "          <se:MinScaleDenominator>{value}</se:MinScaleDenominator>\n"
        ));
    }
    if let Some(value) = rule.maximum_scale_denominator {
        xml.push_str(&format!(
            "          <se:MaxScaleDenominator>{value}</se:MaxScaleDenominator>\n"
        ));
    }
    let has_polygon_symbolizer = rule.symbolizer.fill_image.is_some()
        || rule.symbolizer.fill.is_some()
        || rule.symbolizer.stroke.is_some();
    if has_polygon_symbolizer {
        xml.push_str("          <se:PolygonSymbolizer>\n");
    }
    if let Some(image) = &rule.symbolizer.fill_image {
        xml.push_str("            <se:Fill><se:GraphicFill><se:Graphic><se:ExternalGraphic>\n");
        xml.push_str(&format!(
            "              <se:OnlineResource xlink:type=\"simple\" xlink:href=\"{}\"/>\n",
            escape_xml(image)
        ));
        let format = if image.to_ascii_lowercase().ends_with(".webp") {
            "image/webp"
        } else {
            "image/png"
        };
        xml.push_str(&format!("              <se:Format>{format}</se:Format>\n            </se:ExternalGraphic></se:Graphic></se:GraphicFill></se:Fill>\n"));
    } else if let Some(fill) = &rule.symbolizer.fill {
        xml.push_str("            <se:Fill>\n");
        color_parameter(xml, "fill", fill, 14);
        parameter(
            xml,
            "fill-opacity",
            &rule.symbolizer.fill_opacity.to_string(),
            14,
        );
        xml.push_str("            </se:Fill>\n");
    }
    if let Some(stroke) = &rule.symbolizer.stroke {
        xml.push_str("            <se:Stroke>\n");
        color_parameter(xml, "stroke", stroke, 14);
        parameter(
            xml,
            "stroke-opacity",
            &rule.symbolizer.stroke_opacity.to_string(),
            14,
        );
        parameter(
            xml,
            "stroke-width",
            &rule.symbolizer.stroke_width.to_string(),
            14,
        );
        if !rule.symbolizer.stroke_dasharray.is_empty() {
            parameter(
                xml,
                "stroke-dasharray",
                &rule
                    .symbolizer
                    .stroke_dasharray
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" "),
                14,
            );
        }
        xml.push_str("            </se:Stroke>\n");
    }
    if has_polygon_symbolizer {
        xml.push_str("          </se:PolygonSymbolizer>\n");
    }
    if let Some(label) = &rule.symbolizer.label {
        xml.push_str("          <se:TextSymbolizer>\n            <se:Label><ogc:PropertyName>");
        xml.push_str(&escape_xml(label));
        xml.push_str("</ogc:PropertyName></se:Label>\n            <se:Font>\n");
        parameter(xml, "font-family", &rule.symbolizer.font_family, 14);
        parameter(xml, "font-size", &rule.symbolizer.font_size.to_string(), 14);
        if rule.symbolizer.font_italic {
            parameter(xml, "font-style", "italic", 14);
        }
        if rule.symbolizer.font_bold {
            parameter(xml, "font-weight", "bold", 14);
        }
        xml.push_str("            </se:Font>\n            <se:Halo>\n              <se:Radius>");
        xml.push_str(&rule.symbolizer.halo_radius.to_string());
        xml.push_str("</se:Radius>\n              <se:Fill>\n");
        parameter(xml, "fill", &color_hex(rule.symbolizer.halo_color), 16);
        xml.push_str("              </se:Fill>\n            </se:Halo>\n            <se:Fill>\n");
        parameter(xml, "fill", &color_hex(rule.symbolizer.font_fill), 14);
        xml.push_str("            </se:Fill>\n");
        for (name, value) in [
            ("label-spacing", rule.symbolizer.label_spacing),
            ("label-min-area", rule.symbolizer.label_min_area),
            (
                "label-repeat-distance",
                rule.symbolizer.label_repeat_distance,
            ),
        ] {
            if value > 0.0 {
                xml.push_str(&format!(
                    "            <se:VendorOption name=\"{name}\">{value}</se:VendorOption>\n"
                ));
            }
        }
        if rule.symbolizer.label_repeat_count > 1 {
            xml.push_str(&format!(
                "            <se:VendorOption name=\"label-repeat-count\">{}</se:VendorOption>\n",
                rule.symbolizer.label_repeat_count
            ));
        }
        xml.push_str("          </se:TextSymbolizer>\n");
    }
    xml.push_str("        </se:Rule>\n");
}

fn write_filter(xml: &mut String, filter: &Filter, indent: usize) {
    let pad = " ".repeat(indent);
    match filter {
        Filter::And { filters } => {
            xml.push_str(&format!("{pad}<ogc:And>\n"));
            for child in filters {
                write_filter(xml, child, indent + 2);
            }
            xml.push_str(&format!("{pad}</ogc:And>\n"));
        }
        Filter::Comparison {
            property,
            operator,
            value,
        } => {
            let element = match operator {
                ComparisonOperator::Equal => "PropertyIsEqualTo",
                ComparisonOperator::NotEqual => "PropertyIsNotEqualTo",
                ComparisonOperator::LessThan => "PropertyIsLessThan",
                ComparisonOperator::LessThanOrEqual => "PropertyIsLessThanOrEqualTo",
                ComparisonOperator::GreaterThan => "PropertyIsGreaterThan",
                ComparisonOperator::GreaterThanOrEqual => "PropertyIsGreaterThanOrEqualTo",
            };
            xml.push_str(&format!("{pad}<ogc:{element}>\n{pad}  <ogc:PropertyName>{}</ogc:PropertyName>\n{pad}  <ogc:Literal>{}</ogc:Literal>\n{pad}</ogc:{element}>\n", escape_xml(property), escape_xml(&literal_text(value))));
        }
    }
}

fn parameter(xml: &mut String, name: &str, value: &str, indent: usize) {
    xml.push_str(&format!(
        "{}<se:SvgParameter name=\"{}\">{}</se:SvgParameter>\n",
        " ".repeat(indent),
        name,
        escape_xml(value)
    ));
}
fn color_parameter(xml: &mut String, name: &str, value: &ColorExpression, indent: usize) {
    match value {
        ColorExpression::Literal(color) => parameter(xml, name, &color_hex(*color), indent),
        ColorExpression::Property { property } => {
            let pad = " ".repeat(indent);
            xml.push_str(&format!(
                "{pad}<se:SvgParameter name=\"{}\"><ogc:PropertyName>{}</ogc:PropertyName></se:SvgParameter>\n",
                escape_xml(name),
                escape_xml(property)
            ));
        }
    }
}
fn literal_text(value: &Literal) -> String {
    match value {
        Literal::String(v) => v.clone(),
        Literal::Number(v) => v.to_string(),
        Literal::Boolean(v) => v.to_string(),
    }
}
fn color_hex(c: Rgba) -> String {
    format!("#{:02X}{:02X}{:02X}", c.red, c.green, c.blue)
}
fn escape_xml(value: &str) -> String {
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
    const CSS: &str = "/* parcels */\n[YSDM = '211011'][@scale < 100000] { fill: #58a76f; fill-opacity: 0.8; stroke: #fff; stroke-width: 1.5px; }";

    #[test]
    fn compiles_polygon_geocss_to_ir() {
        let style = compile_geocss("default", CSS).unwrap();
        let rule = &style.rules[0];
        assert_eq!(rule.maximum_scale_denominator, Some(100_000.0));
        assert!(matches!(
            rule.filter,
            Some(Filter::Comparison {
                operator: ComparisonOperator::Equal,
                ..
            })
        ));
        assert!(matches!(
            rule.symbolizer.fill,
            Some(ColorExpression::Literal(Rgba { green: 0xa7, .. }))
        ));
        assert_eq!(rule.symbolizer.stroke_width, 1.5);
    }

    #[test]
    fn serializes_sld_1_1() {
        let sld = to_sld_1_1(&compile_geocss("parcel-default", CSS).unwrap(), "parcels").unwrap();
        assert!(sld.contains("version=\"1.1.0\""));
        assert!(sld.contains("<ogc:PropertyName>YSDM</ogc:PropertyName>"));
        assert!(sld.contains("<se:SvgParameter name=\"fill\">#58A76F"));
        assert!(sld.contains("<se:MaxScaleDenominator>100000"));
        let mut reader = quick_xml::Reader::from_str(&sld);
        loop {
            match reader.read_event().unwrap() {
                quick_xml::events::Event::Eof => break,
                _ => {}
            }
        }
    }

    #[test]
    fn rejects_unknown_property_with_location() {
        let error = compile_geocss("bad", "* { shadow: #000; }").unwrap_err();
        assert!(matches!(error, StyleError::GeoCss { line: 1, .. }));
        assert!(error.to_string().contains("unsupported property `shadow`"));
    }

    #[test]
    fn rejects_invalid_opacity() {
        assert!(compile_geocss("bad", "* { fill: #fff; fill-opacity: 2; }")
            .unwrap_err()
            .to_string()
            .contains("opacity must be between 0 and 1"));
    }

    #[test]
    fn compiles_property_driven_fill_to_sld() {
        let style = compile_geocss("by-color", "* { fill: [color]; stroke: #fff; }").unwrap();
        assert_eq!(
            style.rules[0].symbolizer.fill,
            Some(ColorExpression::Property {
                property: "color".into()
            })
        );
        let sld = to_sld_1_1(&style, "parcels").unwrap();
        assert!(sld
            .contains("<se:SvgParameter name=\"fill\"><ogc:PropertyName>color</ogc:PropertyName>"));
    }

    #[test]
    fn compiles_local_image_fill_and_rejects_unsafe_paths() {
        let style = compile_geocss(
            "pattern",
            "* { fill-image: url('patterns/forest.webp'); fill-image-scale: 1.5; fill-opacity: 0.8; }",
        )
        .unwrap();
        let symbolizer = &style.rules[0].symbolizer;
        assert_eq!(
            symbolizer.fill_image.as_deref(),
            Some("patterns/forest.webp")
        );
        assert_eq!(symbolizer.fill_image_scale, 1.5);
        let sld = to_sld_1_1(&style, "parcels").unwrap();
        assert!(sld.contains("<se:GraphicFill>"));
        assert!(sld.contains("image/webp"));
        assert!(compile_geocss("bad", "* { fill-image: url('../secret.png'); }").is_err());
        assert!(
            compile_geocss("bad", "* { fill-image: url('https://example.com/a.png'); }").is_err()
        );
    }

    #[test]
    fn compiles_field_labels_to_ir_and_sld() {
        let style = compile_geocss(
            "labels",
            "* { label: [name]; font-family: 'Noto Sans CJK SC'; font-size: 16px; font-style: italic; font-fill: #20352a; halo-color: #ffffff; halo-radius: 2px; label-spacing: 8px; label-min-area: 900px; label-repeat-distance: 160px; }",
        )
        .unwrap();
        let symbolizer = &style.rules[0].symbolizer;
        assert_eq!(symbolizer.label.as_deref(), Some("name"));
        assert_eq!(symbolizer.font_size, 16.0);
        assert!(symbolizer.font_italic);
        assert_eq!(symbolizer.halo_radius, 2.0);
        assert_eq!(symbolizer.label_spacing, 8.0);
        assert_eq!(symbolizer.label_min_area, 900.0);
        assert_eq!(symbolizer.label_repeat_distance, 160.0);
        let sld = to_sld_1_1(&style, "parcels").unwrap();
        assert!(sld.contains("<se:TextSymbolizer>"));
        assert!(sld.contains("<ogc:PropertyName>name</ogc:PropertyName>"));
        assert!(sld.contains("<se:SvgParameter name=\"font-family\">Noto Sans CJK SC"));
        assert!(sld.contains("<se:SvgParameter name=\"font-style\">italic"));
        assert!(sld.contains("<se:VendorOption name=\"label-spacing\">8"));
    }
}
