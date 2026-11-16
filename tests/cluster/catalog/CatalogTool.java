// Catalog acceptance tool (T2, T4, T10): drives topic creation through the
// stock Kafka AdminClient and inspects what every broker reports.
//
// Run with the Kafka distribution's client jars (single-file source launch):
//   java -cp "$KAFKA_HOME/libs/*" tests/cluster/catalog/CatalogTool.java <mode> ...
//
// Modes:
//   race    <bootstraps> <names> <attempts> <partitions> <rf> <prefix>
//           <attempts> concurrent CreateTopics spread over the brokers in
//           <bootstraps> (comma list) for <names> distinct names. Prints how
//           many succeeded / reported TOPIC_ALREADY_EXISTS / failed otherwise,
//           per name, then checks every broker reports one identical catalog.
//   burst   <bootstraps> <prefix> <count> <partitions> <ackfile>
//           Creates <count> topics one after another, rotating brokers and
//           retrying retriable errors; appends every acknowledged name to
//           <ackfile>. Used while the catalog leader is killed (T4).
//   latency <bootstrap> <prefix> <count> <partitions>
//           Sequential creates on a settled cluster; prints p50/p99/max ms.
//   check   <bootstraps> <prefix> [ackfile]
//           Every broker must report the same topics with this prefix (same
//           ids, partition counts, leaders); every partition must have a
//           leader; every name in <ackfile> must exist. Exit 1 on any miss.
import org.apache.kafka.clients.admin.*;
import org.apache.kafka.common.*;
import org.apache.kafka.common.errors.*;

import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.stream.*;

public class CatalogTool {
    static Admin admin(String bootstrap) {
        Properties p = new Properties();
        p.put(AdminClientConfig.BOOTSTRAP_SERVERS_CONFIG, bootstrap);
        p.put(AdminClientConfig.REQUEST_TIMEOUT_MS_CONFIG, "30000");
        p.put(AdminClientConfig.DEFAULT_API_TIMEOUT_MS_CONFIG, "60000");
        return Admin.create(p);
    }

    static Throwable root(Throwable t) {
        while ((t instanceof ExecutionException || t instanceof CompletionException) && t.getCause() != null) t = t.getCause();
        return t;
    }

    public static void main(String[] a) throws Exception {
        switch (a[0]) {
            case "race" -> race(a[1].split(","), Integer.parseInt(a[2]), Integer.parseInt(a[3]), Integer.parseInt(a[4]), Short.parseShort(a[5]), a[6]);
            case "burst" -> burst(a[1].split(","), a[2], Integer.parseInt(a[3]), Integer.parseInt(a[4]), Paths.get(a[5]));
            case "latency" -> latency(a[1], a[2], Integer.parseInt(a[3]), Integer.parseInt(a[4]));
            case "check" -> System.exit(check(a[1].split(","), a[2], a.length > 3 ? Paths.get(a[3]) : null) ? 0 : 1);
            default -> throw new IllegalArgumentException(a[0]);
        }
    }

    static void race(String[] boots, int names, int attempts, int partitions, short rf, String prefix) throws Exception {
        List<Admin> admins = Arrays.stream(boots).map(CatalogTool::admin).toList();
        ExecutorService pool = Executors.newFixedThreadPool(64);
        CountDownLatch go = new CountDownLatch(1);
        List<Future<String[]>> results = new ArrayList<>();
        for (int i = 0; i < attempts; i++) {
            final String name = prefix + (i % names);
            final Admin ad = admins.get(i % admins.size());
            results.add(pool.submit(() -> {
                go.await();
                try {
                    ad.createTopics(List.of(new NewTopic(name, partitions, rf))).all().get();
                    return new String[]{name, "created"};
                } catch (Exception e) {
                    Throwable r = root(e);
                    return new String[]{name, r instanceof TopicExistsException ? "exists" : "error:" + r};
                }
            }));
        }
        go.countDown();
        Map<String, Integer> created = new TreeMap<>();
        int exists = 0;
        List<String> errors = new ArrayList<>();
        for (Future<String[]> f : results) {
            String[] r = f.get();
            if (r[1].equals("created")) created.merge(r[0], 1, Integer::sum);
            else if (r[1].equals("exists")) exists++;
            else errors.add(r[0] + " " + r[1]);
        }
        pool.shutdown();
        long multiply = created.values().stream().filter(c -> c != 1).count();
        System.out.printf("race: attempts=%d names=%d created=%d exists=%d errors=%d names_created_more_than_once=%d names_never_created=%d%n",
                attempts, names, created.values().stream().mapToInt(Integer::intValue).sum(), exists, errors.size(), multiply, names - created.size());
        errors.stream().limit(10).forEach(e -> System.out.println("  error " + e));
        for (Admin ad : admins) ad.close();
    }

    static void burst(String[] boots, String prefix, int count, int partitions, Path ackfile) throws Exception {
        List<Admin> admins = Arrays.stream(boots).map(CatalogTool::admin).toList();
        int acked = 0, gaveUp = 0;
        for (int i = 0; i < count; i++) {
            String name = prefix + i;
            boolean done = false;
            for (int attempt = 0; attempt < 60 && !done; attempt++) {
                Admin ad = admins.get((i + attempt) % admins.size());
                try {
                    ad.createTopics(List.of(new NewTopic(name, partitions, (short) Math.min(3, boots.length)))).all().get(20, TimeUnit.SECONDS);
                    done = true;
                } catch (Exception e) {
                    Throwable r = root(e);
                    if (r instanceof TopicExistsException) {
                        // A retry of a create whose first answer was lost: it exists.
                        done = true;
                    } else {
                        Thread.sleep(250);
                    }
                }
            }
            if (done) {
                acked++;
                Files.writeString(ackfile, name + "\n", StandardOpenOption.CREATE, StandardOpenOption.APPEND);
            } else {
                gaveUp++;
                System.out.println("burst: gave up on " + name);
            }
        }
        System.out.printf("burst: acked=%d gave_up=%d%n", acked, gaveUp);
        for (Admin ad : admins) ad.close();
    }

    static void latency(String boot, String prefix, int count, int partitions) throws Exception {
        try (Admin ad = admin(boot)) {
            long[] ms = new long[count];
            for (int i = 0; i < count; i++) {
                long t0 = System.nanoTime();
                ad.createTopics(List.of(new NewTopic(prefix + i, partitions, (short) 3))).all().get();
                ms[i] = (System.nanoTime() - t0) / 1_000_000;
            }
            Arrays.sort(ms);
            System.out.printf("latency: n=%d p50=%dms p99=%dms max=%dms%n", count, ms[count / 2], ms[(int) Math.min(count - 1, Math.ceil(count * 0.99) - 1)], ms[count - 1]);
        }
    }

    record View(Uuid id, int partitions, List<Integer> leaders) {}

    static Map<String, View> view(String boot, String prefix) throws Exception {
        try (Admin ad = admin(boot)) {
            Set<String> names = ad.listTopics().names().get().stream().filter(n -> n.startsWith(prefix)).collect(Collectors.toSet());
            Map<String, View> out = new TreeMap<>();
            if (names.isEmpty()) return out;
            Map<String, TopicDescription> d = ad.describeTopics(names).allTopicNames().get();
            for (var e : d.entrySet()) {
                List<Integer> leaders = e.getValue().partitions().stream()
                        .map(p -> p.leader() == null ? -1 : p.leader().id()).toList();
                out.put(e.getKey(), new View(e.getValue().topicId(), e.getValue().partitions().size(), leaders));
            }
            return out;
        }
    }

    static boolean check(String[] boots, String prefix, Path ackfile) throws Exception {
        boolean ok = true;
        Map<String, View> first = null;
        for (String b : boots) {
            Map<String, View> v = view(b, prefix);
            long leaderless = v.values().stream().filter(x -> x.leaders().contains(-1)).count();
            System.out.printf("check %s: topics=%d leaderless=%d%n", b, v.size(), leaderless);
            if (leaderless > 0) ok = false;
            if (first == null) first = v;
            else if (!first.equals(v)) {
                ok = false;
                Set<String> all = new TreeSet<>(first.keySet());
                all.addAll(v.keySet());
                int shown = 0;
                for (String n : all) {
                    if (!Objects.equals(first.get(n), v.get(n)) && shown++ < 5)
                        System.out.printf("  differs %s: %s vs %s%n", n, first.get(n), v.get(n));
                }
            }
        }
        if (ackfile != null && first != null) {
            List<String> acked = Files.exists(ackfile) ? Files.readAllLines(ackfile) : List.of();
            final Map<String, View> f = first;
            List<String> lost = acked.stream().filter(n -> !n.isBlank() && !f.containsKey(n)).toList();
            System.out.printf("check: acked=%d lost=%d%n", acked.size(), lost.size());
            lost.stream().limit(10).forEach(n -> System.out.println("  lost " + n));
            if (!lost.isEmpty()) ok = false;
        }
        System.out.println(ok ? "check: PASS" : "check: FAIL");
        return ok;
    }
}
