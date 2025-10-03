#!/usr/bin/env python3
"""Test ACL APIs - DescribeAcls, CreateAcls, DeleteAcls"""

from kafka.admin import KafkaAdminClient, ACL, ACLFilter, ResourceType, ResourcePatternType, ACLOperation, ACLPermissionType
import sys
import time

def test_acl_apis():
    """Test ACL operations"""
    try:
        # Create admin client
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            client_id='test-acl-client'
        )

        print("Connected to Kafka admin client")

        # Test 1: Describe ACLs (should see default ACLs)
        print("\n--- Test 1: Describe existing ACLs ---")
        acl_filter = ACLFilter(
            resource_type=ResourceType.ANY,
            resource_pattern_type=ResourcePatternType.ANY,
            operation=ACLOperation.ANY,
            permission_type=ACLPermissionType.ANY
        )

        results = admin.describe_acls(acl_filter)
        print(f"Found {len(results)} ACL(s):")
        for acl in results:
            print(f"  - {acl}")

        # Test 2: Create new ACL
        print("\n--- Test 2: Create new ACL ---")
        new_acl = ACL(
            principal="User:alice",
            host="*",
            resource_type=ResourceType.TOPIC,
            resource_name="test-topic",
            resource_pattern_type=ResourcePatternType.LITERAL,
            operation=ACLOperation.READ,
            permission_type=ACLPermissionType.ALLOW
        )

        create_results = admin.create_acls([new_acl])
        print(f"Created ACL: {create_results}")

        # Test 3: Verify the ACL was created
        print("\n--- Test 3: Verify new ACL exists ---")
        specific_filter = ACLFilter(
            resource_type=ResourceType.TOPIC,
            resource_name="test-topic",
            resource_pattern_type=ResourcePatternType.LITERAL,
            principal="User:alice"
        )

        results = admin.describe_acls(specific_filter)
        print(f"Found {len(results)} matching ACL(s):")
        for acl in results:
            print(f"  - {acl}")

        # Test 4: Delete the ACL
        print("\n--- Test 4: Delete the ACL ---")
        delete_results = admin.delete_acls([specific_filter])
        print(f"Deleted ACLs: {delete_results}")

        # Test 5: Verify deletion
        print("\n--- Test 5: Verify ACL was deleted ---")
        results = admin.describe_acls(specific_filter)
        print(f"Found {len(results)} matching ACL(s) after deletion")

        admin.close()
        print("\n✅ All ACL tests PASSED!")
        return True

    except Exception as e:
        print(f"❌ ACL test failed: {e}")
        print(f"Error type: {type(e)}")
        import traceback
        traceback.print_exc()
        return False

if __name__ == "__main__":
    success = test_acl_apis()
    sys.exit(0 if success else 1)