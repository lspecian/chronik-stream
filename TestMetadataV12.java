import java.net.Socket;
import java.io.OutputStream;
import java.io.InputStream;
import java.nio.ByteBuffer;

public class TestMetadataV12 {
    public static void main(String[] args) throws Exception {
        Socket socket = new Socket("localhost", 9092);
        OutputStream out = socket.getOutputStream();
        InputStream in = socket.getInputStream();
        
        // Send ApiVersions request first
        ByteBuffer apiVersionsReq = ByteBuffer.allocate(20);
        apiVersionsReq.putInt(11); // size
        apiVersionsReq.putShort((short) 18); // ApiVersions
        apiVersionsReq.putShort((short) 3);  // version 3
        apiVersionsReq.putInt(0); // correlation id
        apiVersionsReq.putShort((short) 3); // client_id length
        apiVersionsReq.put("foo".getBytes());
        out.write(apiVersionsReq.array(), 0, 15);
        
        // Read response
        byte[] sizeBytes = new byte[4];
        in.read(sizeBytes);
        int respSize = ByteBuffer.wrap(sizeBytes).getInt();
        byte[] response = new byte[respSize];
        in.read(response);
        System.out.println("ApiVersions response size: " + respSize);
        
        // Send Metadata v12 request
        ByteBuffer metadataReq = ByteBuffer.allocate(30);
        metadataReq.putInt(17); // size
        metadataReq.putShort((short) 3); // Metadata
        metadataReq.putShort((short) 12); // version 12
        metadataReq.putInt(1); // correlation id
        metadataReq.putShort((short) 9); // client_id length
        metadataReq.put("test-java".getBytes());
        out.write(metadataReq.array(), 0, 21);
        
        // Read response size
        in.read(sizeBytes);
        respSize = ByteBuffer.wrap(sizeBytes).getInt();
        System.out.println("Metadata v12 response size: " + respSize);
        
        // Read response
        response = new byte[respSize];
        in.read(response);
        
        // Parse response
        ByteBuffer respBuf = ByteBuffer.wrap(response);
        int correlationId = respBuf.getInt();
        System.out.println("Correlation ID: " + correlationId);
        
        // Throttle time (int32)
        int throttleTime = respBuf.getInt();
        System.out.println("Throttle time: " + throttleTime);
        
        // Brokers array (compact)
        byte brokerCount = respBuf.get();
        System.out.println("Broker count (raw): " + brokerCount);
        System.out.println("Broker count (adjusted): " + (brokerCount - 1));
        
        // Print hex dump of entire response
        System.out.println("\nFull response (hex):");
        for (int i = 0; i < response.length; i++) {
            System.out.printf("%02x ", response[i]);
            if ((i + 1) % 16 == 0) System.out.println();
        }
        System.out.println();
        
        // Try to parse more
        System.out.println("\n\nDetailed parsing:");
        respBuf.position(0);
        System.out.println("Offset 0-3: Correlation ID = " + respBuf.getInt());
        System.out.println("Offset 4-7: Throttle time = " + respBuf.getInt());
        System.out.println("Offset 8: Broker array size = " + respBuf.get());
        System.out.println("Offset 9-12: Node ID = " + respBuf.getInt());
        byte hostLen = respBuf.get();
        System.out.println("Offset 13: Host length = " + hostLen);
        byte[] host = new byte[hostLen - 1];
        respBuf.get(host);
        System.out.println("Host = " + new String(host));
        System.out.println("Port = " + respBuf.getInt());
        System.out.println("Rack (nullable compact string) = " + respBuf.get());
        System.out.println("Tagged fields = " + respBuf.get());
        
        // Continue parsing
        byte clusterIdLen = respBuf.get();
        System.out.println("Cluster ID length = " + clusterIdLen);
        if (clusterIdLen > 0) {
            byte[] clusterId = new byte[clusterIdLen - 1];
            respBuf.get(clusterId);
            System.out.println("Cluster ID = " + new String(clusterId));
        }
        System.out.println("Controller ID = " + respBuf.getInt());
        System.out.println("Topics array size = " + respBuf.get());
        System.out.println("Final tagged fields = " + respBuf.get());
        
        socket.close();
    }
}