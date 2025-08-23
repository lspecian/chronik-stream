import java.net.Socket;
import java.io.OutputStream;
import java.io.InputStream;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;

public class TestProduceDebug {
    public static void main(String[] args) throws Exception {
        // First create topic
        Socket socket = new Socket("localhost", 9092);
        OutputStream out = socket.getOutputStream();
        InputStream in = socket.getInputStream();
        
        // Send simple Produce request v3 (what kafkactl uses)
        ByteBuffer req = ByteBuffer.allocate(200);
        
        // Size placeholder
        req.putInt(0);
        
        // Header
        req.putShort((short) 0); // Produce API
        req.putShort((short) 3); // version 3
        req.putInt(1); // correlation id
        req.putShort((short) 8); // client_id length
        req.put("producer".getBytes());
        
        // Produce request body
        req.putShort((short) -1); // transactional_id (null)
        req.putShort((short) 1); // acks (1 = leader ack)
        req.putInt(1500); // timeout_ms
        
        // Topics array
        req.putInt(1); // 1 topic
        
        // Topic name
        String topicName = "test-debug";
        req.putShort((short) topicName.length());
        req.put(topicName.getBytes());
        
        // Partitions array
        req.putInt(1); // 1 partition
        
        // Partition 0
        req.putInt(0); // partition index
        
        // Records (simplified - null for now)
        req.putInt(-1); // null records
        
        // Update size
        int size = req.position() - 4;
        req.putInt(0, size);
        
        // Send request
        out.write(req.array(), 0, req.position());
        System.out.println("Sent Produce request, size: " + size);
        
        // Read response size
        byte[] sizeBytes = new byte[4];
        in.read(sizeBytes);
        int respSize = ByteBuffer.wrap(sizeBytes).getInt();
        System.out.println("Response size: " + respSize);
        
        // Read response
        byte[] response = new byte[respSize];
        in.read(response);
        
        // Parse response
        ByteBuffer respBuf = ByteBuffer.wrap(response);
        int correlationId = respBuf.getInt();
        System.out.println("Correlation ID: " + correlationId);
        
        // v3 has throttle time first
        int throttleTime = respBuf.getInt();
        System.out.println("Throttle time: " + throttleTime);
        
        // Topics array
        int topicCount = respBuf.getInt();
        System.out.println("Topic count: " + topicCount);
        
        if (topicCount > 0) {
            // Topic name
            short nameLen = respBuf.getShort();
            byte[] nameBytes = new byte[nameLen];
            respBuf.get(nameBytes);
            String returnedTopic = new String(nameBytes);
            System.out.println("Returned topic: " + returnedTopic);
            
            // Partitions
            int partitionCount = respBuf.getInt();
            System.out.println("Partition count: " + partitionCount);
            
            if (partitionCount > 0) {
                int partitionId = respBuf.getInt();
                short errorCode = respBuf.getShort();
                long offset = respBuf.getLong();
                long appendTime = respBuf.getLong(); // v2+
                
                System.out.println("Partition " + partitionId + ": error=" + errorCode + ", offset=" + offset);
                
                if (errorCode == 0) {
                    System.out.println("✅ SUCCESS: Produce working!");
                } else {
                    System.out.println("❌ FAILED: Error code " + errorCode);
                }
            }
        }
        
        socket.close();
    }
}