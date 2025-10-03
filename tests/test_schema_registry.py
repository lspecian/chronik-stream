#!/usr/bin/env python3
"""Test Schema Registry REST API"""

import requests
import json
import sys

SCHEMA_REGISTRY_URL = "http://localhost:8081"

def test_list_subjects():
    """Test listing subjects"""
    print("\n--- Test 1: List subjects (should be empty initially) ---")
    response = requests.get(f"{SCHEMA_REGISTRY_URL}/subjects")
    print(f"Status: {response.status_code}")
    print(f"Response: {response.json()}")
    assert response.status_code == 200
    assert response.json() == []
    print("✅ List subjects test PASSED!")
    return True

def test_register_schema():
    """Test registering a new schema"""
    print("\n--- Test 2: Register a new Avro schema ---")

    schema = {
        "schema": json.dumps({
            "type": "record",
            "name": "User",
            "fields": [
                {"name": "name", "type": "string"},
                {"name": "age", "type": "int"}
            ]
        }),
        "schemaType": "AVRO"
    }

    response = requests.post(
        f"{SCHEMA_REGISTRY_URL}/subjects/user-value/versions",
        json=schema,
        headers={"Content-Type": "application/vnd.schemaregistry.v1+json"}
    )
    print(f"Status: {response.status_code}")
    print(f"Response: {response.json()}")

    assert response.status_code == 200
    data = response.json()
    assert "id" in data
    print(f"✅ Schema registered with ID: {data['id']}")
    return data["id"]

def test_get_schema_by_id(schema_id):
    """Test retrieving schema by ID"""
    print(f"\n--- Test 3: Get schema by ID ({schema_id}) ---")
    response = requests.get(f"{SCHEMA_REGISTRY_URL}/schemas/ids/{schema_id}")
    print(f"Status: {response.status_code}")
    print(f"Response: {response.text}")

    assert response.status_code == 200
    data = response.json()
    assert "schema" in data
    print("✅ Get schema by ID test PASSED!")
    return True

def test_get_latest_version():
    """Test retrieving latest version of a subject"""
    print("\n--- Test 4: Get latest version of 'user-value' ---")
    response = requests.get(f"{SCHEMA_REGISTRY_URL}/subjects/user-value/versions/latest")
    print(f"Status: {response.status_code}")
    print(f"Response: {response.json()}")

    assert response.status_code == 200
    data = response.json()
    assert data["subject"] == "user-value"
    assert data["version"] == 1
    print("✅ Get latest version test PASSED!")
    return True

def test_list_versions():
    """Test listing all versions of a subject"""
    print("\n--- Test 5: List versions of 'user-value' ---")
    response = requests.get(f"{SCHEMA_REGISTRY_URL}/subjects/user-value/versions")
    print(f"Status: {response.status_code}")
    print(f"Response: {response.json()}")

    assert response.status_code == 200
    assert response.json() == [1]
    print("✅ List versions test PASSED!")
    return True

def test_compatibility():
    """Test schema compatibility checking"""
    print("\n--- Test 6: Test compatibility with a new schema ---")

    # New schema with an additional optional field (backward compatible)
    new_schema = {
        "schema": json.dumps({
            "type": "record",
            "name": "User",
            "fields": [
                {"name": "name", "type": "string"},
                {"name": "age", "type": "int"},
                {"name": "email", "type": ["null", "string"], "default": None}
            ]
        }),
        "schemaType": "AVRO"
    }

    response = requests.post(
        f"{SCHEMA_REGISTRY_URL}/compatibility/subjects/user-value/versions/latest",
        json=new_schema,
        headers={"Content-Type": "application/vnd.schemaregistry.v1+json"}
    )
    print(f"Status: {response.status_code}")
    print(f"Response: {response.json()}")

    assert response.status_code == 200
    data = response.json()
    print(f"✅ Compatibility test result: {data}")
    return True

def test_delete_subject():
    """Test deleting a subject"""
    print("\n--- Test 7: Delete subject 'user-value' ---")
    response = requests.delete(f"{SCHEMA_REGISTRY_URL}/subjects/user-value")
    print(f"Status: {response.status_code}")
    print(f"Response: {response.json()}")

    assert response.status_code == 200
    versions = response.json()
    assert 1 in versions
    print(f"✅ Deleted subject with versions: {versions}")
    return True

def test_list_subjects_after_delete():
    """Test listing subjects after deletion"""
    print("\n--- Test 8: List subjects after deletion ---")
    response = requests.get(f"{SCHEMA_REGISTRY_URL}/subjects")
    print(f"Status: {response.status_code}")
    print(f"Response: {response.json()}")

    assert response.status_code == 200
    assert response.json() == []
    print("✅ List subjects after deletion test PASSED!")
    return True

def main():
    """Run all Schema Registry tests"""
    try:
        print("=" * 60)
        print("SCHEMA REGISTRY REST API TESTS")
        print("=" * 60)

        # Test 1: List subjects (empty)
        test_list_subjects()

        # Test 2: Register a schema
        schema_id = test_register_schema()

        # Test 3: Get schema by ID
        test_get_schema_by_id(schema_id)

        # Test 4: Get latest version
        test_get_latest_version()

        # Test 5: List versions
        test_list_versions()

        # Test 6: Test compatibility
        test_compatibility()

        # Test 7: Delete subject
        test_delete_subject()

        # Test 8: List subjects after delete
        test_list_subjects_after_delete()

        print("\n" + "=" * 60)
        print("✅ ALL SCHEMA REGISTRY TESTS PASSED!")
        print("=" * 60)
        return True

    except AssertionError as e:
        print(f"\n❌ Test assertion failed: {e}")
        return False
    except requests.exceptions.ConnectionError:
        print("\n❌ Could not connect to Schema Registry at http://localhost:8081")
        print("Make sure the Schema Registry is running")
        return False
    except Exception as e:
        print(f"\n❌ Unexpected error: {e}")
        import traceback
        traceback.print_exc()
        return False

if __name__ == "__main__":
    success = main()
    sys.exit(0 if success else 1)