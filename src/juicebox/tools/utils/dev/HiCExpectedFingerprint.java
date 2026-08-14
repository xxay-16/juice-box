package juicebox.tools.utils.dev;

import juicebox.HiC;
import juicebox.data.Dataset;
import juicebox.data.DatasetReaderV2;
import juicebox.data.ExpectedValueFunction;
import juicebox.data.ExpectedValueFunctionImpl;
import juicebox.data.basics.ListOfDoubleArrays;
import juicebox.windowui.HiCZoom;
import juicebox.windowui.NormalizationHandler;
import juicebox.windowui.NormalizationType;

import java.lang.reflect.Field;
import java.util.LinkedHashSet;
import java.util.Map;

/** Deterministic expected-value output for Rust migration comparisons. */
public final class HiCExpectedFingerprint {
    private static final long FNV_OFFSET = 0xcbf29ce484222325L;
    private static final long FNV_PRIME = 0x100000001b3L;

    private HiCExpectedFingerprint() {
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            throw new IllegalArgumentException("usage: HiCExpectedFingerprint <file.hic>");
        }
        Dataset dataset = new DatasetReaderV2(args[0]).read();
        int chromosome = 1;
        LinkedHashSet<NormalizationType> types = new LinkedHashSet<>();
        types.add(NormalizationHandler.NONE);
        types.addAll(dataset.getNormalizationTypes());
        for (NormalizationType type : types) {
            for (HiCZoom zoom : dataset.getBpZooms()) {
                ExpectedValueFunction expected = dataset.getExpectedValues(zoom, type);
                if (expected == null) continue;
                ListOfDoubleArrays values = expected.getExpectedValuesNoNormalization();
                if (values == null) continue;
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
                Map<Integer, Double> factors = factors(expected);
                double factor = factors.getOrDefault(chromosome, 1.0);
                double value0 = expected.getExpectedValue(chromosome, 0);
                double pastEnd = expected.getExpectedValue(chromosome, values.getLength() + 1);
                System.out.printf(
                        "type=%s unit=BasePairs resolution=%d values=%d finite=%d sum=%.12f factors=%d factor_chr1=%.12f value0=%.12f past_end=%.12f fingerprint=%016x%n",
                        type, zoom.getBinSize(), values.getLength(), finite, sum,
                        factors.size(), factor, value0, pastEnd, fingerprint);
            }
        }
    }

    @SuppressWarnings("unchecked")
    private static Map<Integer, Double> factors(ExpectedValueFunction expected) throws Exception {
        Field field = ExpectedValueFunctionImpl.class.getDeclaredField("normFactors");
        field.setAccessible(true);
        return (Map<Integer, Double>) field.get(expected);
    }
}
