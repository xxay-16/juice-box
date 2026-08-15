package juicebox.tools.utils.dev;

import juicebox.HiC;
import juicebox.data.ChromosomeHandler;
import juicebox.data.Dataset;
import juicebox.data.DatasetReaderV2;
import juicebox.data.ExpectedValueFunction;
import juicebox.data.Matrix;
import juicebox.data.MatrixZoomData;
import juicebox.mapcolorui.ColorScaleHandler;
import juicebox.mapcolorui.HeatmapRenderer;
import juicebox.mapcolorui.PearsonColorScale;
import juicebox.windowui.HiCZoom;
import juicebox.windowui.MatrixType;
import juicebox.windowui.NormalizationHandler;
import org.broad.igv.renderer.ColorScale;

import java.awt.Color;
import java.awt.Graphics2D;
import java.awt.image.BufferedImage;
import java.util.Locale;

public final class HiCComparisonRenderFingerprint {
    private static final long FNV_OFFSET = 0xcbf29ce484222325L;
    private static final long FNV_PRIME = 0x100000001b3L;

    private HiCComparisonRenderFingerprint() {
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 2) {
            throw new IllegalArgumentException("usage: HiCComparisonRenderFingerprint <observed.hic> <control.hic>");
        }
        Dataset observed = new DatasetReaderV2(args[0]).read();
        Dataset control = new DatasetReaderV2(args[1]).read();
        ChromosomeHandler chromosomes = observed.getChromosomeHandler();
        var chromosome = chromosomes.getChromosomeFromName("1");
        HiCZoom zoom = new HiCZoom(HiC.Unit.BP, 500000);
        Matrix observedMatrix = observed.getMatrix(chromosome, chromosome);
        Matrix controlMatrix = control.getMatrix(chromosome, chromosome);
        MatrixZoomData observedZoom = observedMatrix.getZoomData(zoom);
        MatrixZoomData controlZoom = controlMatrix.getZoomData(zoom);
        ExpectedValueFunction observedExpected = observed.getExpectedValues(zoom, NormalizationHandler.NONE);
        ExpectedValueFunction controlExpected = control.getExpectedValues(zoom, NormalizationHandler.NONE);

        for (MatrixType mode : new MatrixType[]{
                MatrixType.VS, MatrixType.RATIO, MatrixType.RATIOV2,
                MatrixType.OEVS, MatrixType.PEARSONVS,
                MatrixType.OEV2, MatrixType.OECTRLV2, MatrixType.OEVSV2,
                MatrixType.LOG, MatrixType.LOGC, MatrixType.LOGEOVS}) {
            BufferedImage image = new BufferedImage(6, 6, BufferedImage.TYPE_INT_ARGB);
            Graphics2D graphics = image.createGraphics();
            RecordingRenderer renderer = new RecordingRenderer(graphics, new ColorScaleHandler(), 6, 6);
            boolean rendered = renderer.render(
                    0, 0, 6, 6, observedZoom, controlZoom, mode,
                    NormalizationHandler.NONE, NormalizationHandler.NONE,
                    observedExpected, controlExpected, true);
            graphics.dispose();
            if (!rendered) throw new IllegalStateException("renderer rejected " + mode);
            print(mode.name(), renderer.cells());
        }
    }

    private static void print(String mode, float[] cells) {
        long fingerprint = FNV_OFFSET;
        StringBuilder text = new StringBuilder();
        for (int i = 0; i < cells.length; i++) {
            int raw = Float.floatToRawIntBits(cells[i]);
            if (i > 0) text.append(',');
            text.append(String.format(Locale.ROOT, "%08x", raw));
            fingerprint ^= Integer.toUnsignedLong(i);
            fingerprint *= FNV_PRIME;
            fingerprint ^= Integer.toUnsignedLong(raw);
            fingerprint *= FNV_PRIME;
        }
        System.out.printf(Locale.ROOT, "render_mode=%s cells=%d bits=%s fingerprint=%016x%n",
                mode, cells.length, text, fingerprint);
    }

    private static final class RecordingRenderer extends HeatmapRenderer {
        private final int width;
        private final int height;
        private final float[] cells;
        private float pendingScore = Float.NaN;

        RecordingRenderer(Graphics2D graphics, ColorScaleHandler handler, int width, int height) {
            super(graphics, handler);
            this.width = width;
            this.height = height;
            this.cells = new float[width * height];
        }

        float[] cells() {
            return cells;
        }

        @Override
        protected void setScore(float score, ColorScale colorScale) {
            pendingScore = score;
            setColor(Color.BLACK);
        }

        @Override
        protected void setDenseScore(String key, float score, PearsonColorScale pearsonColorScale,
                                     ColorScale genericColorScale) {
            pendingScore = score;
            setColor(Color.BLACK);
        }

        @Override
        protected void directPixelPainting(int px, int py) {
            if (px >= 0 && px < width && py >= 0 && py < height) {
                cells[py * width + px] = pendingScore;
            }
            super.directPixelPainting(px, py);
        }
    }
}
