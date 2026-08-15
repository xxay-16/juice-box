package juicebox.tools.utils.dev;

import juicebox.data.Dataset;
import juicebox.data.DatasetReaderV2;
import juicebox.data.Matrix;
import juicebox.data.MatrixZoomData;
import juicebox.matrix.BasicMatrix;
import juicebox.windowui.HiCZoom;
import juicebox.windowui.NormalizationHandler;
import juicebox.windowui.NormalizationType;

import java.util.Arrays;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Set;

/** Deterministic Pearson output for Rust migration comparisons. */
public final class HiCPearsonFingerprint {
    private static final long FNV_OFFSET = 0xcbf29ce484222325L;
    private static final long FNV_PRIME = 0x100000001b3L;

    private HiCPearsonFingerprint() {
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 1 || args.length > 3) {
            throw new IllegalArgumentException(
                    "usage: HiCPearsonFingerprint <file.hic> [matrix-key] [comma-separated-bin-sizes]");
        }
        String matrixKey = args.length >= 2 ? args[1] : "1_1";
        Set<Integer> requestedBinSizes = parseBinSizes(args.length == 3 ? args[2] : "2500000,1000000");
        DatasetReaderV2 reader = new DatasetReaderV2(args[0]);
        Dataset dataset = reader.read();
        Matrix matrix = reader.readMatrix(matrixKey);
        if (matrix == null) {
            throw new IllegalArgumentException("missing matrix " + matrixKey);
        }

        List<NormalizationType> normalizations = List.of(
                NormalizationHandler.NONE, NormalizationHandler.KR,
                NormalizationHandler.VC, NormalizationHandler.VC_SQRT);
        for (NormalizationType normalization : normalizations) {
            for (HiCZoom zoom : dataset.getBpZooms()) {
                if (!requestedBinSizes.contains(zoom.getBinSize())) continue;
                MatrixZoomData zoomData = matrix.getZoomData(zoom);
                if (zoomData == null || dataset.getExpectedValues(zoom, normalization) == null) continue;
                BasicMatrix pearsons = zoomData.getPearsons(dataset.getExpectedValues(zoom, normalization));
                if (pearsons == null) continue;

                int dimension = pearsons.getRowDimension();
                long finite = 0;
                long nan = 0;
                long infinite = 0;
                double sum = 0;
                long fingerprint = FNV_OFFSET;
                for (int row = 0; row < dimension; row++) {
                    for (int column = 0; column < dimension; column++) {
                        float value = pearsons.getEntry(row, column);
                        if (Float.isFinite(value)) {
                            finite++;
                            sum += value;
                        } else if (Float.isNaN(value)) {
                            nan++;
                        } else {
                            infinite++;
                        }
                        fingerprint = update(fingerprint, Float.floatToRawIntBits(value));
                    }
                }
                int middle = dimension / 2;
                System.out.printf(
                        "norm=%s bin_size=%d dim=%d finite=%d nan=%d infinite=%d sum=%.9f fingerprint=%016x sample_0_0=%08x sample_mid_mid=%08x sample_0_mid=%08x%n",
                        normalization, zoom.getBinSize(), dimension, finite, nan, infinite, sum, fingerprint,
                        Float.floatToRawIntBits(pearsons.getEntry(0, 0)),
                        Float.floatToRawIntBits(pearsons.getEntry(middle, middle)),
                        Float.floatToRawIntBits(pearsons.getEntry(0, middle)));
            }
        }
    }

    private static Set<Integer> parseBinSizes(String text) {
        Set<Integer> sizes = new LinkedHashSet<>();
        Arrays.stream(text.split(","))
                .map(String::trim)
                .filter(value -> !value.isEmpty())
                .map(Integer::parseInt)
                .forEach(sizes::add);
        if (sizes.isEmpty()) {
            throw new IllegalArgumentException("at least one Pearson bin size is required");
        }
        return sizes;
    }

    private static long update(long fingerprint, int value) {
        return (fingerprint ^ Integer.toUnsignedLong(value)) * FNV_PRIME;
    }
}
