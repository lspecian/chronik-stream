# Geo-Spatial Query Support in Chronik Stream

Chronik Stream provides comprehensive geo-spatial query capabilities for location-based search operations. This document describes how to use geo queries in your applications.

## Overview

Geo queries allow you to search for documents based on geographic locations. Chronik Stream supports three types of geo queries:

1. **Geo Distance Query** - Find documents within a specified distance from a point
2. **Geo Bounding Box Query** - Find documents within a rectangular area
3. **Geo Polygon Query** - Find documents within a polygonal area

## Setting Up Geo Fields

To use geo queries, you first need to create an index with a `geo_point` field type:

```json
PUT /places
{
  "mappings": {
    "properties": {
      "name": { "type": "text" },
      "location": { "type": "geo_point" }
    }
  }
}
```

## Indexing Geo Data

You can index geo points in several formats:

### Object Format
```json
PUT /places/_doc/1
{
  "name": "Statue of Liberty",
  "location": {
    "lat": 40.6892,
    "lon": -74.0445
  }
}
```

### Array Format (GeoJSON style - [lon, lat])
```json
PUT /places/_doc/2
{
  "name": "Empire State Building",
  "location": [-73.9857, 40.7484]
}
```

### String Format
```json
PUT /places/_doc/3
{
  "name": "Central Park",
  "location": "40.7829,-73.9654"
}
```

## Geo Query Types

### 1. Geo Distance Query

Find all documents within a specified distance from a central point:

```json
POST /places/_search
{
  "query": {
    "geo_distance": {
      "field": "location",
      "distance": "10km",
      "lat": 40.7128,
      "lon": -74.0060
    }
  }
}
```

**Parameters:**
- `field`: The geo_point field to search
- `distance`: Maximum distance (supports km, mi, m, yd, ft)
- `lat`, `lon`: Center point coordinates
- `distance_type`: (optional) "arc" (default) or "plane"

### 2. Geo Bounding Box Query

Find all documents within a rectangular area:

```json
POST /places/_search
{
  "query": {
    "geo_bounding_box": {
      "field": "location",
      "top_left": {
        "lat": 40.8,
        "lon": -74.1
      },
      "bottom_right": {
        "lat": 40.7,
        "lon": -73.9
      }
    }
  }
}
```

**Parameters:**
- `field`: The geo_point field to search
- `top_left`: Northwestern corner of the box
- `bottom_right`: Southeastern corner of the box

### 3. Geo Polygon Query

Find all documents within a polygonal area:

```json
POST /places/_search
{
  "query": {
    "geo_polygon": {
      "field": "location",
      "points": [
        {"lat": 40.7, "lon": -74.0},
        {"lat": 40.8, "lon": -74.0},
        {"lat": 40.8, "lon": -73.9},
        {"lat": 40.7, "lon": -73.9}
      ]
    }
  }
}
```

**Parameters:**
- `field`: The geo_point field to search
- `points`: Array of points defining the polygon (minimum 3 points)

## Combining Geo Queries

You can combine geo queries with other query types using bool queries:

```json
POST /places/_search
{
  "query": {
    "bool": {
      "must": [
        {
          "match": {
            "category": "restaurant"
          }
        }
      ],
      "filter": [
        {
          "geo_distance": {
            "field": "location",
            "distance": "5km",
            "lat": 40.7580,
            "lon": -73.9855
          }
        }
      ]
    }
  }
}
```

## Distance Units

The following distance units are supported:
- `m` or meters (default)
- `km` or kilometers
- `mi` or miles
- `yd` or yards
- `ft` or feet

## Distance Calculation

Chronik Stream supports two types of distance calculation:

1. **Arc** (default) - Uses the haversine formula for great-circle distance
2. **Plane** - Uses Euclidean distance (faster but less accurate for large distances)

Example with plane distance:
```json
{
  "geo_distance": {
    "field": "location",
    "distance": "10km",
    "distance_type": "plane",
    "lat": 40.7128,
    "lon": -74.0060
  }
}
```

## Performance Considerations

1. **Indexing**: Geo points are internally stored as two separate numeric fields (lat and lon) for efficient range queries
2. **Initial Filtering**: All geo queries use bounding box pre-filtering for performance
3. **Precision**: For polygon and distance queries, documents within the bounding box are checked more precisely
4. **Fast Fields**: Geo fields are stored with fast field access for optimal query performance

## Limitations

1. Currently, only 2D geo queries are supported (no altitude/elevation)
2. Geo shapes (lines, multi-polygons) are not yet supported
3. Geo aggregations are not yet implemented

## Example Use Cases

### Find Nearby Restaurants
```json
{
  "query": {
    "bool": {
      "must": [
        {"term": {"type": "restaurant"}}
      ],
      "filter": [
        {
          "geo_distance": {
            "field": "location",
            "distance": "1km",
            "lat": 40.7128,
            "lon": -74.0060
          }
        }
      ]
    }
  }
}
```

### Find Stores in a Shopping District
```json
{
  "query": {
    "geo_polygon": {
      "field": "location",
      "points": [
        {"lat": 40.7590, "lon": -73.9870},
        {"lat": 40.7570, "lon": -73.9870},
        {"lat": 40.7570, "lon": -73.9840},
        {"lat": 40.7590, "lon": -73.9840}
      ]
    }
  }
}
```

### Find Parks in Manhattan
```json
{
  "query": {
    "bool": {
      "must": [
        {"match": {"amenity": "park"}}
      ],
      "filter": [
        {
          "geo_bounding_box": {
            "field": "location",
            "top_left": {"lat": 40.8820, "lon": -74.0180},
            "bottom_right": {"lat": 40.6980, "lon": -73.9070}
          }
        }
      ]
    }
  }
}
```