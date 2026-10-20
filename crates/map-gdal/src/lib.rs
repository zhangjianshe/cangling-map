//! Compatibility adapter between cangling-map domain types and cangling-gdal.

use cangling_gdal_core::{
    BoundingBox as GdalBoundingBox, DataSource, GdalEngine, ImageFormat as GdalImageFormat,
    VectorComparisonOperator, VectorFilter, VectorLiteral, VectorRenderRequest,
    VectorRenderRule as GdalVectorRenderRule, VectorTextStyle as GdalVectorTextStyle,
};
use map_core::BoundingBox;
use map_render::{
    ImageFormat, RasterizeError, RasterizedImage, VectorColor, VectorRasterRequest,
    VectorRasterizer,
};
use map_style::{ComparisonOperator, Filter, Literal};
use serde::Serialize;

pub use cangling_gdal_core::DatasetKind;

#[derive(Debug, Clone, Default)]
pub struct GdalVectorRasterizer;

impl VectorRasterizer for GdalVectorRasterizer {
    fn render(&self, request: &VectorRasterRequest) -> Result<RasterizedImage, RasterizeError> {
        if !request.bbox.is_valid() || request.width == 0 || request.height == 0 {
            return Err(RasterizeError::InvalidRequest(
                "bbox and image dimensions must be valid".into(),
            ));
        }
        let rgba = |color: map_core::Rgba| [color.red, color.green, color.blue, color.alpha];
        let rules = request
            .rules
            .iter()
            .filter(|rule| {
                rule.minimum_scale_denominator
                    .is_none_or(|minimum| request.scale_denominator >= minimum)
                    && rule
                        .maximum_scale_denominator
                        .is_none_or(|maximum| request.scale_denominator < maximum)
            })
            .map(|rule| {
                Ok(GdalVectorRenderRule {
                    where_clause: rule.filter.as_ref().map(filter_to_ogr_sql).transpose()?,
                    filter: rule.filter.as_ref().map(filter_to_gdal),
                    fill: rule.paint.fill.as_ref().and_then(|color| match color {
                        VectorColor::Literal(value) => Some(rgba(*value)),
                        VectorColor::Property { .. } => None,
                    }),
                    fill_field: rule.paint.fill.as_ref().and_then(|color| match color {
                        VectorColor::Property { property, opacity } => {
                            Some((property.clone(), *opacity))
                        }
                        VectorColor::Literal(_) => None,
                    }),
                    fill_image: rule.paint.fill_image.as_ref().map(|image| {
                        cangling_gdal_core::VectorImageFill {
                            path: image.path.clone(),
                            scale: image.scale,
                            opacity: image.opacity,
                        }
                    }),
                    stroke: rule.paint.stroke.as_ref().and_then(|color| match color {
                        VectorColor::Literal(value) => Some(rgba(*value)),
                        VectorColor::Property { .. } => None,
                    }),
                    stroke_field: rule.paint.stroke.as_ref().and_then(|color| match color {
                        VectorColor::Property { property, opacity } => {
                            Some((property.clone(), *opacity))
                        }
                        VectorColor::Literal(_) => None,
                    }),
                    stroke_width: rule.paint.stroke_width,
                    stroke_dasharray: rule.paint.stroke_dasharray.clone(),
                    text: rule.paint.text.as_ref().map(|text| GdalVectorTextStyle {
                        field: text.field.clone(),
                        font_family: text.font_family.clone(),
                        font_size: text.font_size,
                        font_italic: text.font_italic,
                        font_bold: text.font_bold,
                        fill: rgba(text.fill),
                        halo: rgba(text.halo),
                        halo_radius: text.halo_radius,
                        spacing: text.spacing,
                        min_area: text.min_area,
                        repeat_distance: text.repeat_distance,
                        repeat_count: text.repeat_count,
                    }),
                })
            })
            .collect::<Result<Vec<_>, RasterizeError>>()?;
        let rules = optimize_image_rules(rules);
        let image = GdalEngine
            .render_vector(&VectorRenderRequest {
                source: DataSource {
                    path: request.source.path.clone(),
                    layer: request.source.layer.clone(),
                    encoding: request.source.encoding.clone(),
                },
                target_crs: request.target_crs.clone(),
                bbox: GdalBoundingBox {
                    min_x: request.bbox.min_x,
                    min_y: request.bbox.min_y,
                    max_x: request.bbox.max_x,
                    max_y: request.bbox.max_y,
                },
                width: request.width,
                height: request.height,
                buffer_pixels: request.buffer_pixels,
                simplify_tolerance: request.simplify_tolerance,
                format: match request.format {
                    ImageFormat::Png => GdalImageFormat::Png,
                    ImageFormat::Jpeg => GdalImageFormat::Jpeg,
                },
                rules,
                background: rgba(request.background),
            })
            .map_err(|error| RasterizeError::Backend(error.to_string()))?;
        Ok(RasterizedImage {
            bytes: image.bytes,
            content_type: image.content_type,
        })
    }
}

fn has_fill(rule: &GdalVectorRenderRule) -> bool {
    rule.fill.is_some() || rule.fill_field.is_some() || rule.fill_image.is_some()
}

fn optimize_image_rules(mut rules: Vec<GdalVectorRenderRule>) -> Vec<GdalVectorRenderRule> {
    // Text is an independent symbolizer. Keep its original filter when fill
    // rules are made mutually exclusive; otherwise a base label rule would
    // inherit `NOT (all classified fills)` and match no features.
    let mut text_rules = Vec::new();
    for rule in &mut rules {
        if rule.text.is_some() && has_fill(rule) {
            let mut text_rule = rule.clone();
            text_rule.fill = None;
            text_rule.fill_field = None;
            text_rule.fill_image = None;
            text_rule.stroke = None;
            text_rule.stroke_field = None;
            text_rule.stroke_width = 0.0;
            text_rule.stroke_dasharray.clear();
            text_rules.push(text_rule);
            rule.text = None;
        }
    }
    let mut base_stroke = None;
    for rule in &mut rules {
        if rule.where_clause.is_none() {
            base_stroke = Some((
                rule.stroke,
                rule.stroke_field.clone(),
                rule.stroke_width,
                rule.stroke_dasharray.clone(),
            ));
        } else if has_fill(rule) && rule.stroke.is_none() && rule.stroke_field.is_none() {
            if let Some((stroke, stroke_field, stroke_width, stroke_dasharray)) = &base_stroke {
                rule.stroke = *stroke;
                rule.stroke_field = stroke_field.clone();
                rule.stroke_width = *stroke_width;
                rule.stroke_dasharray = stroke_dasharray.clone();
            }
        }
    }

    for index in 0..rules.len() {
        if !has_fill(&rules[index]) {
            continue;
        }
        let later_fills = rules[index + 1..]
            .iter()
            .filter(|rule| has_fill(rule))
            .filter_map(|rule| rule.where_clause.as_deref())
            .map(|filter| format!("({filter})"))
            .collect::<Vec<_>>();
        let later_filters = rules[index + 1..]
            .iter()
            .filter(|rule| has_fill(rule))
            .filter_map(|rule| rule.filter.clone())
            .collect::<Vec<_>>();
        if later_fills.is_empty() {
            continue;
        }
        let exclusion = format!("NOT ({})", later_fills.join(" OR "));
        rules[index].where_clause = Some(match rules[index].where_clause.take() {
            Some(current) => format!("({current}) AND {exclusion}"),
            None => exclusion,
        });
        if !later_filters.is_empty() {
            let exclusion = VectorFilter::Not {
                filter: Box::new(if later_filters.len() == 1 {
                    later_filters.into_iter().next().expect("one filter")
                } else {
                    VectorFilter::Or {
                        filters: later_filters,
                    }
                }),
            };
            rules[index].filter = Some(match rules[index].filter.take() {
                Some(current) => VectorFilter::And {
                    filters: vec![current, exclusion],
                },
                None => exclusion,
            });
        }
    }
    rules.extend(text_rules);
    rules
}

fn filter_to_gdal(filter: &Filter) -> VectorFilter {
    match filter {
        Filter::Comparison {
            property,
            operator,
            value,
        } => VectorFilter::Comparison {
            property: property.clone(),
            operator: match operator {
                ComparisonOperator::Equal => VectorComparisonOperator::Equal,
                ComparisonOperator::NotEqual => VectorComparisonOperator::NotEqual,
                ComparisonOperator::LessThan => VectorComparisonOperator::LessThan,
                ComparisonOperator::LessThanOrEqual => VectorComparisonOperator::LessThanOrEqual,
                ComparisonOperator::GreaterThan => VectorComparisonOperator::GreaterThan,
                ComparisonOperator::GreaterThanOrEqual => {
                    VectorComparisonOperator::GreaterThanOrEqual
                }
            },
            value: match value {
                Literal::String(value) => VectorLiteral::String(value.clone()),
                Literal::Number(value) => VectorLiteral::Number(*value),
                Literal::Boolean(value) => VectorLiteral::Boolean(*value),
            },
        },
        Filter::And { filters } => VectorFilter::And {
            filters: filters.iter().map(filter_to_gdal).collect(),
        },
    }
}

fn filter_to_ogr_sql(filter: &Filter) -> Result<String, RasterizeError> {
    match filter {
        Filter::Comparison {
            property,
            operator,
            value,
        } => {
            if property.is_empty()
                || !property
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return Err(RasterizeError::InvalidRequest(format!(
                    "invalid filter property: {property}"
                )));
            }
            let operator = match operator {
                ComparisonOperator::Equal => "=",
                ComparisonOperator::NotEqual => "<>",
                ComparisonOperator::LessThan => "<",
                ComparisonOperator::LessThanOrEqual => "<=",
                ComparisonOperator::GreaterThan => ">",
                ComparisonOperator::GreaterThanOrEqual => ">=",
            };
            let value = match value {
                Literal::String(value) => format!("'{}'", value.replace('\'', "''")),
                Literal::Number(value) if value.is_finite() => value.to_string(),
                Literal::Number(_) => {
                    return Err(RasterizeError::InvalidRequest(
                        "filter number must be finite".into(),
                    ))
                }
                Literal::Boolean(value) => {
                    if *value {
                        "TRUE".into()
                    } else {
                        "FALSE".into()
                    }
                }
            };
            Ok(format!("\"{property}\" {operator} {value}"))
        }
        Filter::And { filters } => filters
            .iter()
            .map(filter_to_ogr_sql)
            .collect::<Result<Vec<_>, _>>()
            .map(|parts| format!("({})", parts.join(" AND "))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_rule(where_clause: Option<&str>, path: &str) -> GdalVectorRenderRule {
        GdalVectorRenderRule {
            where_clause: where_clause.map(str::to_owned),
            filter: None,
            fill: None,
            fill_field: None,
            fill_image: Some(cangling_gdal_core::VectorImageFill {
                path: path.into(),
                scale: 1.0,
                opacity: 1.0,
            }),
            stroke: None,
            stroke_field: None,
            stroke_width: 1.0,
            stroke_dasharray: Vec::new(),
            text: None,
        }
    }

    #[test]
    fn translates_nested_filters_to_safe_ogr_sql() {
        let filter = Filter::And {
            filters: vec![
                Filter::Comparison {
                    property: "kind".into(),
                    operator: ComparisonOperator::Equal,
                    value: Literal::String("rock'fall".into()),
                },
                Filter::Comparison {
                    property: "level".into(),
                    operator: ComparisonOperator::GreaterThanOrEqual,
                    value: Literal::Number(3.0),
                },
            ],
        };
        assert_eq!(
            filter_to_ogr_sql(&filter).unwrap(),
            "(\"kind\" = 'rock''fall' AND \"level\" >= 3)"
        );
    }

    #[test]
    fn makes_ordered_image_rules_mutually_exclusive_and_inherits_base_stroke() {
        let mut base = image_rule(None, "base.png");
        base.stroke = Some([12, 34, 56, 255]);
        base.stroke_width = 2.0;
        base.text = Some(GdalVectorTextStyle {
            field: "name".into(),
            font_family: "Noto Sans CJK SC".into(),
            font_size: 14.0,
            font_italic: false,
            font_bold: false,
            fill: [0, 0, 0, 255],
            halo: [255, 255, 255, 255],
            halo_radius: 2.0,
            spacing: 8.0,
            min_area: 900.0,
            repeat_distance: 160.0,
            repeat_count: 1,
        });
        let mut missing_image_fallback = image_rule(Some("\"type\" = 2"), "missing.png");
        missing_image_fallback.fill_image = None;
        missing_image_fallback.fill = Some([176, 176, 176, 255]);
        let rules = optimize_image_rules(vec![
            base,
            image_rule(Some("\"type\" = 1"), "one.png"),
            missing_image_fallback,
        ]);

        assert_eq!(
            rules[0].where_clause.as_deref(),
            Some("NOT ((\"type\" = 1) OR (\"type\" = 2))")
        );
        assert_eq!(
            rules[1].where_clause.as_deref(),
            Some("(\"type\" = 1) AND NOT ((\"type\" = 2))")
        );
        assert_eq!(rules[1].stroke, Some([12, 34, 56, 255]));
        assert_eq!(rules[1].stroke_width, 2.0);
        assert_eq!(rules.len(), 4);
        assert!(rules[0].text.is_none());
        assert_eq!(rules[3].where_clause, None);
        assert_eq!(
            rules[3].text.as_ref().map(|text| text.field.as_str()),
            Some("name")
        );
        assert!(!has_fill(&rules[3]));
    }
}

#[derive(Debug, Serialize)]
pub struct SourceInspection {
    pub native_crs: String,
    pub native_extent: BoundingBox,
}

pub fn preview_vector_features(
    path: &str,
    layer_name: Option<&str>,
    encoding: Option<&str>,
    limit: usize,
) -> Result<cangling_gdal_core::VectorFeaturePreview, String> {
    GdalEngine
        .preview_vector_features(
            &DataSource {
                path: path.into(),
                layer: layer_name.map(str::to_owned),
                encoding: encoding.map(str::to_owned),
            },
            limit,
        )
        .map_err(|error| error.to_string())
}

pub fn vector_field_stats(
    path: &str,
    layer_name: Option<&str>,
    encoding: Option<&str>,
    field: &str,
) -> Result<cangling_gdal_core::VectorFieldStats, String> {
    GdalEngine
        .vector_field_stats(
            &DataSource {
                path: path.into(),
                layer: layer_name.map(str::to_owned),
                encoding: encoding.map(str::to_owned),
            },
            field,
            500,
        )
        .map_err(|error| error.to_string())
}

pub fn inspect_source(
    path: &str,
    kind: DatasetKind,
    layer_name: Option<&str>,
    encoding: Option<&str>,
) -> Result<SourceInspection, String> {
    let info = GdalEngine
        .inspect(
            &DataSource {
                path: path.into(),
                layer: layer_name.map(str::to_owned),
                encoding: encoding.map(str::to_owned),
            },
            kind,
        )
        .map_err(|error| error.to_string())?;
    Ok(SourceInspection {
        native_crs: info.native_crs,
        native_extent: BoundingBox {
            min_x: info.native_extent.min_x,
            min_y: info.native_extent.min_y,
            max_x: info.native_extent.max_x,
            max_y: info.native_extent.max_y,
        },
    })
}
