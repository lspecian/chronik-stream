import java.net.Socket;
import java.nio.ByteBuffer;
import java.io.InputStream;
import java.io.OutputStream;

public class TestCreateTopic {
    public static void main(String[] args) throws Exception {
        Socket socket = new Socket("localhost", 9092);
        OutputStream out = socket.getOutputStream();
        InputStream in = socket.getInputStream();
        
        // Build CreateTopics request v0
        ByteBuffer request = ByteBuffer.allocate(256);
        request.putShort((short) 19);  // API key (CreateTopics)
        request.putShort((short) 0);   // API version 0
        request.putInt(100);            // Correlation ID
        request.putShort((short) -1);  // Client ID (null)
        
        // CreateTopics body
        request.putInt(1);              // 1 topic to create
        
        // Topic name
        String topicName = "test-topic";
        request.putShort((short) topicName.length());
        request.put(topicName.getBytes());
        
        // Partitions
        request.putInt(3);              // 3 partitions
        
        // Replication factor
        request.putShort((short) 1);    // replication factor 1
        
        // Replica assignments (empty array for auto-assignment)
        request.putInt(0);              // 0 assignments
        
        // Config entries (empty for v0)
        request.putInt(0);              // 0 configs
        
        // Timeout
        request.putInt(30000);          // 30 second timeout
        
        // Send request
        int requestSize = request.position();
        ByteBuffer sizeBuffer = ByteBuffer.allocate(4);
        sizeBuffer.putInt(requestSize);
        out.write(sizeBuffer.array());
        out.write(request.array(), 0, requestSize);
        out.flush();
        
        System.out.println("Sent CreateTopics request for topic: " + topicName);
        
        // Read response size
        byte[] sizeBytes = new byte[4];
        in.read(sizeBytes);
        int size = ByteBuffer.wrap(sizeBytes).getInt();
        System.out.println("Response size: " + size);
        
        // Read response
        byte[] response = new byte[size];
        int totalRead = 0;
        while (totalRead < size) {
            int read = in.read(response, totalRead, size - totalRead);
            if (read < 0) break;
            totalRead += read;
        }
        
        // Parse response
        ByteBuffer respBuffer = ByteBuffer.wrap(response);
        int correlationId = respBuffer.getInt();
        System.out.println("Correlation ID: " + correlationId);
        
        // Topic errors array
        int topicCount = respBuffer.getInt();
        System.out.println("Topic count in response: " + topicCount);
        
        if (topicCount > 0) {
            // Topic name
            short nameLen = respBuffer.getShort();
            byte[] nameBytes = new byte[nameLen];
            respBuffer.get(nameBytes);
            String returnedName = new String(nameBytes);
            
            // Error code
            short errorCode = respBuffer.getShort();
            
            System.out.println("Topic: " + returnedName + ", Error code: " + errorCode);
            
            if (errorCode == 0) {
                System.out.println("SUCCESS: Topic created successfully!");
            } else {
                System.out.println("Failed with error code: " + errorCode);
            }
        }
        
        socket.close();
        
        // Now test Metadata again to see if topic appears
        System.out.println("\nFetching metadata to verify topic exists...");
        socket = new Socket("localhost", 9092);
        out = socket.getOutputStream();
        in = socket.getInputStream();
        
        // Metadata request
        request = ByteBuffer.allocate(14);
        request.putShort((short) 3);  // Metadata
        request.putShort((short) 0);  // v0
        request.putInt(101);           // Correlation ID
        request.putShort((short) -1); // Client ID
        request.putInt(-1);           // All topics
        
        sizeBuffer = ByteBuffer.allocate(4);
        sizeBuffer.putInt(request.position());
        out.write(sizeBuffer.array());
        out.write(request.array(), 0, request.position());
        out.flush();
        
        // Read response
        in.read(sizeBytes);
        size = ByteBuffer.wrap(sizeBytes).getInt();
        response = new byte[size];
        totalRead = 0;
        while (totalRead < size) {
            int read = in.read(response, totalRead, size - totalRead);
            if (read < 0) break;
            totalRead += read;
        }
        
        respBuffer = ByteBuffer.wrap(response);
        correlationId = respBuffer.getInt();
        
        // Skip brokers
        int brokersLen = respBuffer.getInt();
        for (int i = 0; i < brokersLen; i++) {
            respBuffer.getInt(); // node id
            short hostLen = respBuffer.getShort();
            respBuffer.position(respBuffer.position() + hostLen); // skip host
            respBuffer.getInt(); // port
        }
        
        // Topics
        int topicsLen = respBuffer.getInt();
        System.out.println("Topics found: " + topicsLen);
        
        for (int i = 0; i < topicsLen; i++) {
            short errorCode = respBuffer.getShort();
            short nameLen = respBuffer.getShort();
            byte[] nameBytes = new byte[nameLen];
            respBuffer.get(nameBytes);
            String name = new String(nameBytes);
            System.out.println("  - " + name + " (error: " + errorCode + ")");
            
            // Skip partitions
            int partitionsLen = respBuffer.getInt();
            for (int j = 0; j < partitionsLen; j++) {
                respBuffer.getShort(); // error
                respBuffer.getInt(); // partition id
                respBuffer.getInt(); // leader
                
                // Replicas array
                int replicasLen = respBuffer.getInt();
                for (int k = 0; k < replicasLen; k++) {
                    respBuffer.getInt();
                }
                
                // ISR array
                int isrLen = respBuffer.getInt();
                for (int k = 0; k < isrLen; k++) {
                    respBuffer.getInt();
                }
            }
        }
        
        socket.close();
    }
}