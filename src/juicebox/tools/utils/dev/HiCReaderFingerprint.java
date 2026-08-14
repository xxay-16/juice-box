package juicebox.tools.utils.dev;

import juicebox.HiC;
import juicebox.data.Block;
import juicebox.data.ContactRecord;
import juicebox.data.Dataset;
import juicebox.data.DatasetReaderV2;
import juicebox.data.ExpectedValueFunction;
import juicebox.data.Matrix;
import juicebox.data.MatrixZoomData;
import juicebox.windowui.HiCZoom;
import juicebox.windowui.NormalizationHandler;
import juicebox.windowui.NormalizationType;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/** Deterministic raw-reader output for Rust migration comparisons. */
public final class HiCReaderFingerprint {
    private static final long FNV_OFFSET = 0xcbf29ce484222325L;
    private static final long FNV_PRIME = 0x100000001b3L;

    private HiCReaderFingerprint() {
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 1 || args.length > 2) {
            throw new IllegalArgumentException("usage: HiCReaderFingerprint <file.hic> [matrix-key]");
        }
        String key = args.length == 2 ? args[1] : "1_1";
        DatasetReaderV2 reader = new DatasetReaderV2(args[0]);
        Dataset dataset = reader.read();
        Matrix matrix = reader.readMatrix(key);
        if (matrix == null) {
            throw new IllegalArgumentException("missing matrix " + key);
        }
        List<NormalizationType> normalizations = List.of(
                NormalizationHandler.NONE, NormalizationHandler.KR,
                NormalizationHandler.VC, NormalizationHandler.VC_SQRT);
        for (NormalizationType normalization : normalizations) {
            for (HiCZoom zoom : dataset.getBpZooms()) {
                MatrixZoomData zoomData = matrix.getZoomData(zoom);
                if (zoomData == null) continue;
                List<Integer> blockNumbers = new ArrayList<>(zoomData.getBlockNumbers());
                Collections.sort(blockNumbers);
                long records = 0;
                long finite = 0;
                double storedCountSum = 0;
                long fingerprint = FNV_OFFSET;
                for (int blockNumber : blockNumbers) {
                    Block block = reader.readNormalizedBlock(blockNumber, zoomData, normalization);
                    if (block == null) continue;
                    for (ContactRecord record : block.getContactRecords()) {
                        records++;
                        if (Float.isFinite(record.getCounts())) {
                            finite++;
                            storedCountSum += record.getCounts();
                        }
                        fingerprint = update(fingerprint, record.getBinX());
                        fingerprint = update(fingerprint, record.getBinY());
                        fingerprint = update(fingerprint, Float.floatToRawIntBits(record.getCounts()));
                    }
                }
                System.out.printf(
                        "norm=%s unit=%s bin_size=%d blocks=%d records=%d finite=%d stored_count_sum=%.9f fingerprint=%016x%n",
                        normalization, HiC.Unit.BP, zoom.getBinSize(), blockNumbers.size(), records, finite, storedCountSum, fingerprint);

                ExpectedValueFunction expected = dataset.getExpectedValues(zoom, normalization);
                if (expected != null) {
                    long oeRecords = 0;
                    long oeFinite = 0;
                    double oeSum = 0;
                    long oeFingerprint = FNV_OFFSET;
                    for (int blockNumber : blockNumbers) {
                        Block block = reader.readNormalizedBlock(blockNumber, zoomData, normalization);
                        if (block == null) continue;
                        for (ContactRecord record : block.getContactRecords()) {
                            int distance = Math.abs(record.getBinX() - record.getBinY());
                            float ratio = (float) (record.getCounts()
                                    / expected.getExpectedValue(zoomData.getChr1Idx(), distance));
                            if (Float.isNaN(ratio)) continue;
                            oeRecords++;
                            if (Float.isFinite(ratio)) {
                                oeFinite++;
                                oeSum += ratio;
                            }
                            oeFingerprint = update(oeFingerprint, record.getBinX());
                            oeFingerprint = update(oeFingerprint, record.getBinY());
                            oeFingerprint = update(oeFingerprint, Float.floatToRawIntBits(ratio));
                        }
                    }
                    System.out.printf(
                            "oe_norm=%s unit=%s bin_size=%d records=%d finite=%d sum=%.9f fingerprint=%016x%n",
                            normalization, HiC.Unit.BP, zoom.getBinSize(), oeRecords, oeFinite, oeSum, oeFingerprint);
                }
            }
        }
    }

    private static long update(long fingerprint, int value) {
        return (fingerprint ^ Integer.toUnsignedLong(value)) * FNV_PRIME;
    }
}
