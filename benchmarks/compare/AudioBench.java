import com.github.natanbc.lavadsp.timescale.TimescalePcmAudioFilter;
import com.sedmelluq.discord.lavaplayer.filter.AudioFilter;
import com.sedmelluq.discord.lavaplayer.filter.PcmFilterFactory;
import com.sedmelluq.discord.lavaplayer.filter.equalizer.EqualizerFactory;
import com.sedmelluq.discord.lavaplayer.format.AudioDataFormat;
import com.sedmelluq.discord.lavaplayer.format.StandardAudioDataFormats;
import com.sedmelluq.discord.lavaplayer.player.AudioConfiguration;
import com.sedmelluq.discord.lavaplayer.player.AudioPlayer;
import com.sedmelluq.discord.lavaplayer.player.DefaultAudioPlayerManager;
import com.sedmelluq.discord.lavaplayer.source.local.LocalAudioSourceManager;
import com.sedmelluq.discord.lavaplayer.track.AudioItem;
import com.sedmelluq.discord.lavaplayer.track.AudioReference;
import com.sedmelluq.discord.lavaplayer.track.AudioTrack;
import com.sedmelluq.discord.lavaplayer.track.playback.MutableAudioFrame;

import java.nio.ByteBuffer;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Callable;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.locks.LockSupport;

/** Exact Lavalink 4.2.2/Lavaplayer audio workload for run.py. */
public final class AudioBench {
    private static final long FRAME_NANOS = 20_000_000L;
    private static final long LATENCY_BUCKET_US = 100L;
    private static final int LATENCY_BUCKETS = 100_001;

    private AudioBench() {}

    public static void main(String[] raw) throws Exception {
        Map<String, String> args = arguments(raw);
        Path input = Path.of(required(args, "--input")).toAbsolutePath().normalize();
        String filter = required(args, "--filter");
        String mode = required(args, "--mode");
        int concurrency = Integer.parseInt(required(args, "--concurrency"));
        long warmup = Long.parseLong(required(args, "--warmup-seconds"));
        long measure = Long.parseLong(required(args, "--measure-seconds"));
        if (concurrency <= 0 || measure <= 0) {
            throw new IllegalArgumentException("concurrency and measurement duration must be positive");
        }
        if (!mode.equals("throughput") && !mode.equals("realtime")) {
            throw new IllegalArgumentException("--mode must be throughput or realtime");
        }

        DefaultAudioPlayerManager manager = new DefaultAudioPlayerManager();
        manager.registerSourceManager(new LocalAudioSourceManager());
        manager.setFrameBufferDuration(5_000);
        AudioConfiguration configuration = manager.getConfiguration();
        configuration.setOutputFormat(StandardAudioDataFormats.DISCORD_OPUS);
        configuration.setResamplingQuality(AudioConfiguration.ResamplingQuality.LOW);
        configuration.setOpusEncodingQuality(10);

        AudioItem item = manager.loadItemSync(new AudioReference(input.toString(), null));
        if (!(item instanceof AudioTrack)) {
            manager.shutdown();
            throw new IllegalStateException("fixture did not load as a track: " + input);
        }
        AudioTrack master = (AudioTrack) item;
        long start = System.nanoTime() + 100_000_000L;
        long measureStart = start + warmup * 1_000_000_000L;
        long end = measureStart + measure * 1_000_000_000L;

        ExecutorService executor = Executors.newFixedThreadPool(concurrency);
        List<Future<Stats>> futures = new ArrayList<>();
        for (int i = 0; i < concurrency; i++) {
            futures.add(executor.submit(new Worker(
                manager, master, filter, mode, start, measureStart, end, (int) measure
            )));
        }

        sleepUntil(measureStart);
        System.out.println("BENCHMARK_MEASURE");
        System.out.flush();

        Stats total = new Stats((int) measure);
        for (Future<Stats> future : futures) {
            total.merge(future.get());
        }
        executor.shutdownNow();
        manager.shutdown();

        long frames = total.frames();
        long bytes = total.bytes();
        long misses = total.misses();
        double audioRate = frames * 0.02 / measure;
        double bitrate = frames == 0 ? 0.0 : bytes * 8.0 * 50.0 / frames;
        System.out.println("{" +
            "\"implementation\":\"java\"," +
            "\"workload\":\"audio\"," +
            "\"case\":\"" + filter + "\"," +
            "\"mode\":\"" + mode + "\"," +
            "\"concurrency\":" + concurrency + "," +
            "\"wall_seconds\":" + measure + "," +
            "\"frames\":" + frames + "," +
            "\"bytes\":" + bytes + "," +
            "\"misses\":" + misses + "," +
            "\"p99_service_us\":" + total.percentile(0.99) + "," +
            "\"audio_seconds_per_wall_second\":" + audioRate + "," +
            "\"output_bitrate\":" + bitrate + "," +
            "\"windows\":" + total.windowsJson() +
            "}");
    }

    private static final class Worker implements Callable<Stats> {
        private final DefaultAudioPlayerManager manager;
        private final AudioTrack master;
        private final String filter;
        private final String mode;
        private final long start;
        private final long measureStart;
        private final long end;
        private final int seconds;

        private Worker(
            DefaultAudioPlayerManager manager,
            AudioTrack master,
            String filter,
            String mode,
            long start,
            long measureStart,
            long end,
            int seconds
        ) {
            this.manager = manager;
            this.master = master;
            this.filter = filter;
            this.mode = mode;
            this.start = start;
            this.measureStart = measureStart;
            this.end = end;
            this.seconds = seconds;
        }

        @Override
        public Stats call() throws Exception {
            AudioPlayer player = manager.createPlayer();
            player.setFilterFactory(filterFactory(filter));
            MutableAudioFrame frame = new MutableAudioFrame();
            frame.setBuffer(ByteBuffer.allocate(StandardAudioDataFormats.DISCORD_OPUS.maximumChunkSize()));
            Stats stats = new Stats(seconds);
            boolean realtime = mode.equals("realtime");
            long nextFrame = start;
            sleepUntil(start);

            while (System.nanoTime() < end) {
                if (player.getPlayingTrack() == null) {
                    player.playTrack(master.makeClone());
                }
                if (realtime) {
                    sleepUntil(nextFrame);
                    nextFrame += FRAME_NANOS;
                }
                long serviceStart = System.nanoTime();
                boolean provided = player.provide(frame);
                long finished = System.nanoTime();
                if (finished < measureStart) {
                    if (!provided) Thread.yield();
                    continue;
                }
                int index = windowIndex(finished, measureStart, seconds);
                if (provided) {
                    stats.windows[index][0]++;
                    stats.windows[index][1] += frame.getDataLength();
                } else {
                    if (realtime) stats.windows[index][2]++;
                    Thread.yield();
                }
                stats.recordLatency((finished - serviceStart) / 1_000L);
            }
            player.destroy();
            return stats;
        }
    }

    private static PcmFilterFactory filterFactory(String name) {
        if (name.equals("default")) return null;
        if (name.equals("eq")) {
            EqualizerFactory equalizer = new EqualizerFactory();
            equalizer.setGain(0, 0.15f);
            equalizer.setGain(7, -0.10f);
            equalizer.setGain(14, 0.20f);
            return equalizer;
        }
        if (name.equals("timescale")) {
            return (track, format, output) -> {
                TimescalePcmAudioFilter timescale = new TimescalePcmAudioFilter(
                    output, format.channelCount, format.sampleRate
                );
                timescale.setSpeed(1.10).setPitch(1.05).setRate(1.0);
                return Collections.<AudioFilter>singletonList(timescale);
            };
        }
        throw new IllegalArgumentException("unknown filter case: " + name);
    }

    private static final class Stats {
        private final long[][] windows;
        private final long[] latency = new long[LATENCY_BUCKETS];

        private Stats(int seconds) {
            windows = new long[Math.max(seconds, 1)][3];
        }

        private void recordLatency(long micros) {
            int bucket = (int) Math.min(micros / LATENCY_BUCKET_US, LATENCY_BUCKETS - 1L);
            latency[bucket]++;
        }

        private void merge(Stats source) {
            for (int i = 0; i < windows.length; i++) {
                for (int value = 0; value < 3; value++) windows[i][value] += source.windows[i][value];
            }
            for (int i = 0; i < latency.length; i++) latency[i] += source.latency[i];
        }

        private long frames() { return sum(0); }
        private long bytes() { return sum(1); }
        private long misses() { return sum(2); }

        private long sum(int value) {
            long total = 0;
            for (long[] window : windows) total += window[value];
            return total;
        }

        private long percentile(double quantile) {
            long total = 0;
            for (long count : latency) total += count;
            if (total == 0) return 0;
            long wanted = (long) Math.ceil(total * quantile);
            long seen = 0;
            for (int i = 0; i < latency.length; i++) {
                seen += latency[i];
                if (seen >= wanted) return i * LATENCY_BUCKET_US;
            }
            return (latency.length - 1L) * LATENCY_BUCKET_US;
        }

        private String windowsJson() {
            StringBuilder json = new StringBuilder("[");
            for (int i = 0; i < windows.length; i++) {
                if (i > 0) json.append(',');
                json.append("{\"frames\":").append(windows[i][0])
                    .append(",\"bytes\":").append(windows[i][1])
                    .append(",\"misses\":").append(windows[i][2])
                    .append('}');
            }
            return json.append(']').toString();
        }
    }

    private static Map<String, String> arguments(String[] raw) {
        Map<String, String> values = new HashMap<>();
        for (int i = 0; i < raw.length; i += 2) {
            if (i + 1 >= raw.length) throw new IllegalArgumentException("missing value for " + raw[i]);
            values.put(raw[i], raw[i + 1]);
        }
        return values;
    }

    private static String required(Map<String, String> args, String name) {
        String value = args.get(name);
        if (value == null) throw new IllegalArgumentException("missing " + name);
        return value;
    }

    private static int windowIndex(long now, long start, int length) {
        long second = Math.max(0L, now - start) / 1_000_000_000L;
        return (int) Math.min(second, length - 1L);
    }

    private static void sleepUntil(long deadline) {
        long remaining;
        while ((remaining = deadline - System.nanoTime()) > 0) LockSupport.parkNanos(remaining);
    }
}
