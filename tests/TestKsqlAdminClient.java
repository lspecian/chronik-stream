import org.apache.kafka.clients.admin.AdminClient;
import org.apache.kafka.clients.admin.AdminClientConfig;
import org.apache.kafka.clients.admin.DescribeClusterResult;
import java.util.Properties;
import java.util.concurrent.TimeUnit;

public class TestKsqlAdminClient {
    public static void main(String[] args) {
        Properties props = new Properties();
        props.put(AdminClientConfig.BOOTSTRAP_SERVERS_CONFIG, "localhost:9092");
        props.put(AdminClientConfig.CLIENT_ID_CONFIG, "test-admin-java");
        props.put(AdminClientConfig.REQUEST_TIMEOUT_MS_CONFIG, 5000);

        System.out.println("Creating AdminClient...");
        AdminClient admin = AdminClient.create(props);

        try {
            System.out.println("Describing cluster...");
            DescribeClusterResult result = admin.describeCluster();

            System.out.println("Getting cluster ID...");
            String clusterId = result.clusterId().get(5, TimeUnit.SECONDS);
            System.out.println("Cluster ID: " + clusterId);

            System.out.println("Getting nodes...");
            var nodes = result.nodes().get(5, TimeUnit.SECONDS);
            System.out.println("Nodes: " + nodes.size());
            for (var node : nodes) {
                System.out.println("  Node " + node.id() + ": " + node.host() + ":" + node.port());
            }

            System.out.println("SUCCESS!");
        } catch (Exception e) {
            System.err.println("ERROR: " + e.getMessage());
            e.printStackTrace();
        } finally {
            admin.close();
        }
    }
}