package juicebox.tools.utils.dev;

import juicebox.HiC;
import juicebox.data.Dataset;
import juicebox.data.DatasetReaderV2;
import juicebox.data.NormalizationVector;
import juicebox.data.basics.ListOfDoubleArrays;
import juicebox.windowui.HiCZoom;
import juicebox.windowui.NormalizationType;

/** Deterministic normalization-vector output for Rust migration comparisons. */
public final class HiCNormalizationFingerprint {
    private static final long FNV_OFFSET = 0xcbf29ce484222325L;
    private static final long FNV_PRIME = 0x100000001b3L;

    private HiCNormalizationFingerprint() {
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            throw new IllegalArgumentException("usage: HiCNormalizationFingerprint <file.hic>");
        }
        DatasetReaderV2 reader = new DatasetReaderV2(args[0]);
        Dataset dataset = reader.read();
        int chromosome = 1;
        for (NormalizationType type : dataset.getNormalizationTypes()) {
            for (HiCZoom zoom : dataset.getBpZooms()) {
                NormalizationVector vector = reader.readNormalizationVector(
                        type, chromosome, HiC.Unit.BP, zoom.getBinSize());
                if (vector == null) continue;
                ListOfDoubleArrays values = vector.getData();
                long finite = 0;
                double sum = 0;
                long fingerprint = FNV_OFFSET;
                for (long index = 0; index < values.getLength(); index++) {
                    double value = values.get(index);
                    if (Double.isFinite(value)) {
                        finite++;
                        sum += value;
                    }
                    fingerprint = (fingerprint ^ Double.doubleToRawLongBits(value)) * FNV_PRIME;
                }
                System.out.printf(
                        "type=%s chr=%d unit=BasePairs resolution=%d values=%d finite=%d sum=%.12f fingerprint=%016x%n",
                        type, chromosome, zoom.getBinSize(), values.getLength(), finite, sum, fingerprint);
            }
        }
    }
}
