package juicebox.tools.utils.dev;

import juicebox.HiC;
import juicebox.data.Block;
import juicebox.data.ContactRecord;
import juicebox.data.Dataset;
import juicebox.data.DatasetReaderV2;
import juicebox.data.Matrix;
import juicebox.data.MatrixZoomData;
import juicebox.windowui.HiCZoom;
import juicebox.windowui.NormalizationHandler;

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
        for (HiCZoom zoom : dataset.getBpZooms()) {
            MatrixZoomData zoomData = matrix.getZoomData(zoom);
            if (zoomData == null) continue;
            List<Integer> blockNumbers = new ArrayList<>(zoomData.getBlockNumbers());
            Collections.sort(blockNumbers);
            long records = 0;
            double storedCountSum = 0;
            long fingerprint = FNV_OFFSET;
            for (int blockNumber : blockNumbers) {
                Block block = reader.readNormalizedBlock(blockNumber, zoomData, NormalizationHandler.NONE);
                for (ContactRecord record : block.getContactRecords()) {
                    records++;
                    storedCountSum += record.getCounts();
                    fingerprint = update(fingerprint, record.getBinX());
                    fingerprint = update(fingerprint, record.getBinY());
                    fingerprint = update(fingerprint, Float.floatToRawIntBits(record.getCounts()));
                }
            }
            System.out.printf(
                    "unit=%s bin_size=%d blocks=%d records=%d stored_count_sum=%.9f fingerprint=%016x%n",
                    HiC.Unit.BP, zoom.getBinSize(), blockNumbers.size(), records, storedCountSum, fingerprint);
        }
    }

    private static long update(long fingerprint, int value) {
        return (fingerprint ^ Integer.toUnsignedLong(value)) * FNV_PRIME;
    }
}
