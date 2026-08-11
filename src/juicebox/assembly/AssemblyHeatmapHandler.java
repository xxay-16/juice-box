/*
 * The MIT License (MIT)
 *
 * Copyright (c) 2011-2020 Broad Institute, Aiden Lab, Rice University, Baylor College of Medicine
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 *  THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 *  IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 *  FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 *  AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 *  LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 *  OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
 *  THE SOFTWARE.
 */

package juicebox.assembly;

import juicebox.HiCGlobals;
import juicebox.data.Block;
import juicebox.data.ContactRecord;
import juicebox.gui.SuperAdapter;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Created by muhammadsaadshamim on 4/17/17.
 */
public class AssemblyHeatmapHandler {

    private static final int MAX_LOCAL_BIN_MAP_SIZE = 262_144;
    private static final int MAX_BIN_MAP_ENTRIES_PER_RECORD = 4;
    private static final long SLOW_ASSEMBLY_MAPPING_NANOS = 100_000_000L;
    private static final long SLOW_MAPPING_LOG_INTERVAL_NANOS = 1_000_000_000L;
    private static final AtomicLong lastSlowMappingLogNanos = new AtomicLong();
    private static SuperAdapter superAdapter;
    private static volatile AssemblyCoordinateMapper coordinateMapper = AssemblyCoordinateMapper.empty();

    public static void setListOfOSortedAggregateScaffolds(List<Scaffold> listOfAggregateScaffolds) {
        List<Scaffold> sortedScaffolds = new ArrayList<>(listOfAggregateScaffolds);
        Collections.sort(sortedScaffolds, Scaffold.originalStateComparator);
        coordinateMapper = new AssemblyCoordinateMapper(sortedScaffolds);
    }

    public static SuperAdapter getSuperAdapter() {
        return AssemblyHeatmapHandler.superAdapter;
    }

    public static void setSuperAdapter(SuperAdapter superAdapter) {
        AssemblyHeatmapHandler.superAdapter = superAdapter;
    }

    public static Block modifyBlock(Block block, String key, int binSize, int chr1Idx, int chr2Idx) {
        long mappingStartNanos = System.nanoTime();
        //temp fix for AllByAll. TODO: trace this!
        if (chr1Idx == 0 && chr2Idx == 0) {
            binSize = 1000 * binSize; // AllByAll is measured in kb
        }

        List<ContactRecord> records = block.getContactRecords();
        if (records.isEmpty()) {
            return new Block(block.getNumber(), new ArrayList<>(), key);
        }

        double scaledBinSize = HiCGlobals.hicMapScale * binSize;
        AssemblyCoordinateMapper mapper = coordinateMapper;
        BinRangeMapper xBinMapper = BinRangeMapper.create(records, true, scaledBinSize, mapper);
        BinRangeMapper yBinMapper = BinRangeMapper.create(records, false, scaledBinSize, mapper);

        List<ContactRecord> alteredContacts = new ArrayList<>(records.size());
        for (ContactRecord record : records) {

            int alteredAsmBinX = xBinMapper.map(record.getBinX());
            int alteredAsmBinY = yBinMapper.map(record.getBinY());

            if (alteredAsmBinX == -1 || alteredAsmBinY == -1) {
                alteredContacts.add(record);
            } else {
                int mappedBinX = Math.min(alteredAsmBinX, alteredAsmBinY);
                int mappedBinY = Math.max(alteredAsmBinX, alteredAsmBinY);
                if (mappedBinX == record.getBinX() && mappedBinY == record.getBinY()) {
                    alteredContacts.add(record);
                    continue;
                }
                if (alteredAsmBinX > alteredAsmBinY) {
                    alteredContacts.add(new ContactRecord(
                            alteredAsmBinY,
                            alteredAsmBinX, record.getCounts()));
                } else {
                    alteredContacts.add(new ContactRecord(
                            alteredAsmBinX,
                            alteredAsmBinY, record.getCounts()));
                }
            }
        }
        block = new Block(block.getNumber(), alteredContacts, key);
        maybeLogSlowMapping(System.nanoTime() - mappingStartNanos, records.size(), binSize);
        return block;
    }

    private static void maybeLogSlowMapping(long mappingNanos, int recordCount, int binSize) {
        if (mappingNanos < SLOW_ASSEMBLY_MAPPING_NANOS) {
            return;
        }
        long now = System.nanoTime();
        long previous = lastSlowMappingLogNanos.get();
        if (now - previous < SLOW_MAPPING_LOG_INTERVAL_NANOS
                || !lastSlowMappingLogNanos.compareAndSet(previous, now)) {
            return;
        }
        System.err.printf("Slow assembly mapping: %.1f ms, records=%d, binSize=%d%n",
                mappingNanos / 1_000_000.0, recordCount, binSize);
    }



    private static final class BinRangeMapper {
        private final int firstBin;
        private final int[] mappedBins;
        private final double scaledBinSize;
        private final AssemblyCoordinateMapper mapper;

        private BinRangeMapper(int firstBin, int[] mappedBins, double scaledBinSize, AssemblyCoordinateMapper mapper) {
            this.firstBin = firstBin;
            this.mappedBins = mappedBins;
            this.scaledBinSize = scaledBinSize;
            this.mapper = mapper;
        }

        private static BinRangeMapper create(List<ContactRecord> records, boolean useX, double scaledBinSize,
                                             AssemblyCoordinateMapper mapper) {
            int minBin = Integer.MAX_VALUE;
            int maxBin = Integer.MIN_VALUE;
            for (ContactRecord record : records) {
                int bin = useX ? record.getBinX() : record.getBinY();
                minBin = Math.min(minBin, bin);
                maxBin = Math.max(maxBin, bin);
            }

            long rangeSize = (long) maxBin - minBin + 1;
            long usefulRangeLimit = Math.max(256L, (long) records.size() * MAX_BIN_MAP_ENTRIES_PER_RECORD);
            if (rangeSize > MAX_LOCAL_BIN_MAP_SIZE || rangeSize > usefulRangeLimit) {
                return new BinRangeMapper(0, null, scaledBinSize, mapper);
            }

            int[] mappedBins = new int[(int) rangeSize];
            for (int offset = 0; offset < mappedBins.length; offset++) {
                mappedBins[offset] = mapper.mapBin(minBin + offset, scaledBinSize);
            }
            return new BinRangeMapper(minBin, mappedBins, scaledBinSize, mapper);
        }

        private int map(int bin) {
            if (mappedBins == null) {
                return mapper.mapBin(bin, scaledBinSize);
            }
            return mappedBins[bin - firstBin];
        }
    }

    private static final class AssemblyCoordinateMapper {
        private final long[] originalStarts;
        private final long[] currentStarts;
        private final long[] currentEnds;
        private final boolean[] inverted;

        private AssemblyCoordinateMapper(List<Scaffold> sortedScaffolds) {
            int size = sortedScaffolds.size();
            originalStarts = new long[size];
            currentStarts = new long[size];
            currentEnds = new long[size];
            inverted = new boolean[size];
            for (int i = 0; i < size; i++) {
                Scaffold scaffold = sortedScaffolds.get(i);
                originalStarts[i] = scaffold.getOriginalStart();
                currentStarts[i] = scaffold.getCurrentStart();
                currentEnds[i] = scaffold.getCurrentEnd();
                inverted[i] = scaffold.getInvertedVsInitial();
            }
        }

        private static AssemblyCoordinateMapper empty() {
            return new AssemblyCoordinateMapper(Collections.emptyList());
        }

        private int mapBin(int binValue, double scaledBinSize) {
            long originalFirstNucleotide = (long) (binValue * scaledBinSize + 1);
            int scaffoldIndex = findScaffoldIndex(originalFirstNucleotide);
            if (scaffoldIndex < 0) {
                return -1;
            }

            long currentFirstNucleotide;
            if (!inverted[scaffoldIndex]) {
                currentFirstNucleotide = currentStarts[scaffoldIndex] + originalFirstNucleotide - originalStarts[scaffoldIndex];
            } else {
                currentFirstNucleotide = currentEnds[scaffoldIndex] - originalFirstNucleotide + 2
                        - (long) scaledBinSize + originalStarts[scaffoldIndex];
            }
            return (int) ((currentFirstNucleotide - 1) / scaledBinSize);
        }

        private int findScaffoldIndex(long genomicPosition) {
            int low = 0;
            int high = originalStarts.length;
            while (low < high) {
                int mid = (low + high) >>> 1;
                if (originalStarts[mid] <= genomicPosition) {
                    low = mid + 1;
                } else {
                    high = mid;
                }
            }
            return low - 1;
        }

    }
}
