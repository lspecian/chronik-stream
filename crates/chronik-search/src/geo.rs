//! Geo-spatial query support for Chronik Stream search

use chronik_common::{Result, Error};
use serde::{Deserialize, Serialize};
use tantivy::{
    query::{Query, BooleanQuery, RangeQuery, Occur},
    schema::{Field, Schema, FAST, STORED},
    Term,
};

/// Geographic point representation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoPoint {
    pub lat: f64,
    pub lon: f64,
}

impl GeoPoint {
    /// Create a new geo point
    pub fn new(lat: f64, lon: f64) -> Result<Self> {
        if lat < -90.0 || lat > 90.0 {
            return Err(Error::InvalidInput(format!("Invalid latitude: {}", lat)));
        }
        if lon < -180.0 || lon > 180.0 {
            return Err(Error::InvalidInput(format!("Invalid longitude: {}", lon)));
        }
        Ok(Self { lat, lon })
    }
}

/// Distance calculation type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DistanceType {
    /// Great circle distance (accurate)
    Arc,
    /// Euclidean distance (faster, less accurate)
    Plane,
}

impl Default for DistanceType {
    fn default() -> Self {
        Self::Arc
    }
}

/// Bounding box for geo queries
#[derive(Debug, Clone)]
pub struct GeoBoundingBox {
    pub top_left: GeoPoint,
    pub bottom_right: GeoPoint,
}

/// Geo query types
#[derive(Debug, Clone)]
pub enum GeoQuery {
    /// Distance query (circle around a point)
    Distance {
        field: String,
        center: GeoPoint,
        distance: f64, // in meters
        distance_type: DistanceType,
    },
    /// Bounding box query
    BoundingBox {
        field: String,
        top_left: GeoPoint,
        bottom_right: GeoPoint,
    },
    /// Polygon query
    Polygon {
        field: String,
        points: Vec<GeoPoint>,
    },
}

/// Geo field mapping in schema
pub struct GeoFieldMapping {
    lat_field: Field,
    lon_field: Field,
}

/// Extension trait for Schema to support geo fields
pub trait GeoSchemaExt {
    fn add_geo_point_field(&mut self, field_name: &str) -> (Field, Field);
}

impl GeoSchemaExt for tantivy::schema::SchemaBuilder {
    fn add_geo_point_field(&mut self, field_name: &str) -> (Field, Field) {
        // Tantivy doesn't have native geo support, so we use two f64 fields
        let lat_field = self.add_f64_field(
            &format!("{}_lat", field_name),
            FAST | STORED,
        );
        let lon_field = self.add_f64_field(
            &format!("{}_lon", field_name),
            FAST | STORED,
        );
        
        (lat_field, lon_field)
    }
}

impl GeoQuery {
    /// Convert geo query to Tantivy query
    pub fn to_tantivy_query(&self, schema: &Schema) -> Result<Box<dyn Query>> {
        match self {
            GeoQuery::Distance { field, center, distance, distance_type: _ } => {
                // Convert to bounding box for initial filtering
                let bbox = calculate_bounding_box(center, *distance)?;
                let lat_field = schema.get_field(&format!("{}_lat", field))
                    .map_err(|_| Error::InvalidInput(format!("Geo field {} not found", field)))?;
                let lon_field = schema.get_field(&format!("{}_lon", field))
                    .map_err(|_| Error::InvalidInput(format!("Geo field {} not found", field)))?;
                
                // Create range queries for bounding box
                let lat_lower = Term::from_field_f64(lat_field, bbox.bottom_right.lat);
                let lat_upper = Term::from_field_f64(lat_field, bbox.top_left.lat);
                let lat_query = RangeQuery::new(
                    std::ops::Bound::Included(lat_lower),
                    std::ops::Bound::Included(lat_upper),
                );
                
                let lon_lower = Term::from_field_f64(lon_field, bbox.top_left.lon);
                let lon_upper = Term::from_field_f64(lon_field, bbox.bottom_right.lon);
                let lon_query = RangeQuery::new(
                    std::ops::Bound::Included(lon_lower),
                    std::ops::Bound::Included(lon_upper),
                );
                
                // Combine queries
                let bool_query = BooleanQuery::new(vec![
                    (Occur::Must, Box::new(lat_query)),
                    (Occur::Must, Box::new(lon_query)),
                ]);
                
                // For now, return the bounding box query
                // In production, you'd wrap this with a custom scorer for precise distance
                Ok(Box::new(bool_query))
            },
            
            GeoQuery::BoundingBox { field, top_left, bottom_right } => {
                let lat_field = schema.get_field(&format!("{}_lat", field))
                    .map_err(|_| Error::InvalidInput(format!("Geo field {} not found", field)))?;
                let lon_field = schema.get_field(&format!("{}_lon", field))
                    .map_err(|_| Error::InvalidInput(format!("Geo field {} not found", field)))?;
                
                let lat_lower = Term::from_field_f64(lat_field, bottom_right.lat);
                let lat_upper = Term::from_field_f64(lat_field, top_left.lat);
                let lat_query = RangeQuery::new(
                    std::ops::Bound::Included(lat_lower),
                    std::ops::Bound::Included(lat_upper),
                );
                
                let lon_lower = Term::from_field_f64(lon_field, top_left.lon);
                let lon_upper = Term::from_field_f64(lon_field, bottom_right.lon);
                let lon_query = RangeQuery::new(
                    std::ops::Bound::Included(lon_lower),
                    std::ops::Bound::Included(lon_upper),
                );
                
                let bool_query = BooleanQuery::new(vec![
                    (Occur::Must, Box::new(lat_query)),
                    (Occur::Must, Box::new(lon_query)),
                ]);
                
                Ok(Box::new(bool_query))
            },
            
            GeoQuery::Polygon { field, points } => {
                if points.len() < 3 {
                    return Err(Error::InvalidInput("Polygon must have at least 3 points".to_string()));
                }
                
                // Use bounding box of polygon for initial filtering
                let bbox = calculate_polygon_bbox(points)?;
                let lat_field = schema.get_field(&format!("{}_lat", field))
                    .map_err(|_| Error::InvalidInput(format!("Geo field {} not found", field)))?;
                let lon_field = schema.get_field(&format!("{}_lon", field))
                    .map_err(|_| Error::InvalidInput(format!("Geo field {} not found", field)))?;
                
                let lat_lower = Term::from_field_f64(lat_field, bbox.bottom_right.lat);
                let lat_upper = Term::from_field_f64(lat_field, bbox.top_left.lat);
                let lat_query = RangeQuery::new(
                    std::ops::Bound::Included(lat_lower),
                    std::ops::Bound::Included(lat_upper),
                );
                
                let lon_lower = Term::from_field_f64(lon_field, bbox.top_left.lon);
                let lon_upper = Term::from_field_f64(lon_field, bbox.bottom_right.lon);
                let lon_query = RangeQuery::new(
                    std::ops::Bound::Included(lon_lower),
                    std::ops::Bound::Included(lon_upper),
                );
                
                let bool_query = BooleanQuery::new(vec![
                    (Occur::Must, Box::new(lat_query)),
                    (Occur::Must, Box::new(lon_query)),
                ]);
                
                // For now, return the bounding box query
                // In production, you'd wrap this with a custom scorer for point-in-polygon test
                Ok(Box::new(bool_query))
            },
        }
    }
}

/// Calculate distance between two points
pub fn calculate_distance(p1: &GeoPoint, p2: &GeoPoint, distance_type: DistanceType) -> f64 {
    match distance_type {
        DistanceType::Arc => haversine_distance(p1, p2),
        DistanceType::Plane => euclidean_distance(p1, p2),
    }
}

/// Haversine distance calculation (great circle distance)
fn haversine_distance(p1: &GeoPoint, p2: &GeoPoint) -> f64 {
    const EARTH_RADIUS_M: f64 = 6_371_000.0;
    
    let lat1_rad = p1.lat.to_radians();
    let lat2_rad = p2.lat.to_radians();
    let delta_lat = (p2.lat - p1.lat).to_radians();
    let delta_lon = (p2.lon - p1.lon).to_radians();
    
    let a = (delta_lat / 2.0).sin().powi(2)
        + lat1_rad.cos() * lat2_rad.cos() * (delta_lon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    
    EARTH_RADIUS_M * c
}

/// Euclidean distance (faster but less accurate)
fn euclidean_distance(p1: &GeoPoint, p2: &GeoPoint) -> f64 {
    const METERS_PER_DEGREE_LAT: f64 = 111_319.9;
    
    let lat_diff = p2.lat - p1.lat;
    let lon_diff = p2.lon - p1.lon;
    
    // Adjust longitude difference by latitude
    let avg_lat = (p1.lat + p2.lat) / 2.0;
    let meters_per_degree_lon = METERS_PER_DEGREE_LAT * avg_lat.to_radians().cos();
    
    let lat_meters = lat_diff * METERS_PER_DEGREE_LAT;
    let lon_meters = lon_diff * meters_per_degree_lon;
    
    (lat_meters * lat_meters + lon_meters * lon_meters).sqrt()
}

/// Calculate bounding box for a distance query
fn calculate_bounding_box(center: &GeoPoint, distance_meters: f64) -> Result<GeoBoundingBox> {
    const EARTH_RADIUS_M: f64 = 6_371_000.0;
    const METERS_PER_DEGREE_LAT: f64 = 111_319.9;
    
    // Calculate latitude difference
    let lat_diff = distance_meters / METERS_PER_DEGREE_LAT;
    
    // Calculate longitude difference (varies by latitude)
    let meters_per_degree_lon = METERS_PER_DEGREE_LAT * center.lat.to_radians().cos();
    let lon_diff = if meters_per_degree_lon > 0.0 {
        distance_meters / meters_per_degree_lon
    } else {
        180.0 // At poles, use maximum longitude difference
    };
    
    let top_lat = (center.lat + lat_diff).min(90.0);
    let bottom_lat = (center.lat - lat_diff).max(-90.0);
    let left_lon = center.lon - lon_diff;
    let right_lon = center.lon + lon_diff;
    
    // Handle longitude wrap-around
    let (left_lon, right_lon) = if left_lon < -180.0 || right_lon > 180.0 {
        (-180.0, 180.0) // Full longitude range
    } else {
        (left_lon, right_lon)
    };
    
    Ok(GeoBoundingBox {
        top_left: GeoPoint::new(top_lat, left_lon)?,
        bottom_right: GeoPoint::new(bottom_lat, right_lon)?,
    })
}

/// Calculate bounding box for a polygon
fn calculate_polygon_bbox(points: &[GeoPoint]) -> Result<GeoBoundingBox> {
    if points.is_empty() {
        return Err(Error::InvalidInput("Empty polygon".to_string()));
    }
    
    let mut min_lat = points[0].lat;
    let mut max_lat = points[0].lat;
    let mut min_lon = points[0].lon;
    let mut max_lon = points[0].lon;
    
    for point in points.iter().skip(1) {
        min_lat = min_lat.min(point.lat);
        max_lat = max_lat.max(point.lat);
        min_lon = min_lon.min(point.lon);
        max_lon = max_lon.max(point.lon);
    }
    
    Ok(GeoBoundingBox {
        top_left: GeoPoint::new(max_lat, min_lon)?,
        bottom_right: GeoPoint::new(min_lat, max_lon)?,
    })
}

/// Check if a point is inside a polygon using ray casting algorithm
pub fn point_in_polygon(point: &GeoPoint, polygon: &[GeoPoint]) -> bool {
    if polygon.len() < 3 {
        return false;
    }
    
    let mut inside = false;
    let mut j = polygon.len() - 1;
    
    for i in 0..polygon.len() {
        let xi = polygon[i].lon;
        let yi = polygon[i].lat;
        let xj = polygon[j].lon;
        let yj = polygon[j].lat;
        
        let intersect = ((yi > point.lat) != (yj > point.lat))
            && (point.lon < (xj - xi) * (point.lat - yi) / (yj - yi) + xi);
            
        if intersect {
            inside = !inside;
        }
        
        j = i;
    }
    
    inside
}

/// Parse distance string (e.g., "10km", "5mi", "100m")
pub fn parse_distance_string(distance_str: &str) -> Result<f64> {
    let distance_str = distance_str.trim().to_lowercase();
    
    // Extract numeric part and unit
    let (number_part, unit_part): (String, String) = distance_str
        .chars()
        .partition(|c| c.is_numeric() || *c == '.');
    
    let distance_value = number_part.parse::<f64>()
        .map_err(|_| Error::InvalidInput(format!("Invalid distance: {}", distance_str)))?;
    
    // Convert to meters based on unit
    let distance_meters = match unit_part.as_str() {
        "m" | "" => distance_value,
        "km" => distance_value * 1000.0,
        "mi" => distance_value * 1609.34,
        "yd" => distance_value * 0.9144,
        "ft" => distance_value * 0.3048,
        _ => return Err(Error::InvalidInput(format!("Unknown distance unit: {}", unit_part))),
    };
    
    Ok(distance_meters)
}

/// Parse geo point from various formats
pub fn parse_geo_point(value: &serde_json::Value) -> Result<GeoPoint> {
    // Support various formats
    if let Some(obj) = value.as_object() {
        // Object format: {"lat": 40.7128, "lon": -74.0060}
        if let (Some(lat), Some(lon)) = (obj.get("lat"), obj.get("lon")) {
            let lat = lat.as_f64()
                .ok_or_else(|| Error::InvalidInput("Invalid latitude".to_string()))?;
            let lon = lon.as_f64()
                .ok_or_else(|| Error::InvalidInput("Invalid longitude".to_string()))?;
            return GeoPoint::new(lat, lon);
        }
    } else if let Some(arr) = value.as_array() {
        // Array format: [lon, lat] (GeoJSON style)
        if arr.len() == 2 {
            let lon = arr[0].as_f64()
                .ok_or_else(|| Error::InvalidInput("Invalid longitude".to_string()))?;
            let lat = arr[1].as_f64()
                .ok_or_else(|| Error::InvalidInput("Invalid latitude".to_string()))?;
            return GeoPoint::new(lat, lon);
        }
    } else if let Some(s) = value.as_str() {
        // String format: "lat,lon"
        let parts: Vec<&str> = s.split(',').collect();
        if parts.len() == 2 {
            let lat = parts[0].trim().parse::<f64>()
                .map_err(|_| Error::InvalidInput("Invalid latitude".to_string()))?;
            let lon = parts[1].trim().parse::<f64>()
                .map_err(|_| Error::InvalidInput("Invalid longitude".to_string()))?;
            return GeoPoint::new(lat, lon);
        }
    }
    
    Err(Error::InvalidInput("Invalid geo point format".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_geo_point_validation() {
        assert!(GeoPoint::new(0.0, 0.0).is_ok());
        assert!(GeoPoint::new(90.0, 180.0).is_ok());
        assert!(GeoPoint::new(-90.0, -180.0).is_ok());
        
        assert!(GeoPoint::new(91.0, 0.0).is_err());
        assert!(GeoPoint::new(-91.0, 0.0).is_err());
        assert!(GeoPoint::new(0.0, 181.0).is_err());
        assert!(GeoPoint::new(0.0, -181.0).is_err());
    }
    
    #[test]
    fn test_haversine_distance() {
        // New York to Los Angeles
        let ny = GeoPoint::new(40.7128, -74.0060).unwrap();
        let la = GeoPoint::new(34.0522, -118.2437).unwrap();
        
        let distance = haversine_distance(&ny, &la);
        
        // Expected distance is approximately 3935 km
        assert!((distance - 3_935_000.0).abs() < 10_000.0);
    }
    
    #[test]
    fn test_parse_distance_string() {
        assert_eq!(parse_distance_string("100m").unwrap(), 100.0);
        assert_eq!(parse_distance_string("5km").unwrap(), 5000.0);
        assert_eq!(parse_distance_string("10mi").unwrap(), 16093.4);
        assert_eq!(parse_distance_string("100").unwrap(), 100.0);
    }
    
    #[test]
    fn test_point_in_polygon() {
        // Simple triangle
        let polygon = vec![
            GeoPoint::new(0.0, 0.0).unwrap(),
            GeoPoint::new(0.0, 1.0).unwrap(),
            GeoPoint::new(1.0, 0.0).unwrap(),
        ];
        
        // Point inside triangle
        assert!(point_in_polygon(&GeoPoint::new(0.25, 0.25).unwrap(), &polygon));
        
        // Point outside triangle
        assert!(!point_in_polygon(&GeoPoint::new(1.0, 1.0).unwrap(), &polygon));
    }
    
    #[test]
    fn test_parse_geo_point() {
        // Object format
        let obj = serde_json::json!({"lat": 40.7128, "lon": -74.0060});
        let point = parse_geo_point(&obj).unwrap();
        assert_eq!(point.lat, 40.7128);
        assert_eq!(point.lon, -74.0060);
        
        // Array format (GeoJSON)
        let arr = serde_json::json!([-74.0060, 40.7128]);
        let point = parse_geo_point(&arr).unwrap();
        assert_eq!(point.lat, 40.7128);
        assert_eq!(point.lon, -74.0060);
        
        // String format
        let s = serde_json::json!("40.7128,-74.0060");
        let point = parse_geo_point(&s).unwrap();
        assert_eq!(point.lat, 40.7128);
        assert_eq!(point.lon, -74.0060);
    }
}