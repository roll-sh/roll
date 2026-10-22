//! Static types.
//!
//! A [`Ty`] is a class id plus an array dimension count, small enough to copy everywhere. Class
//! ids below [`cls::BUILTIN_COUNT`] are fixed at compile time so the analyzer, the VM and the
//! library code can name `java.lang.String` or `java.util.List` without a lookup; their members
//! live in the registry ([`crate::lookup`]). `void`, `def` and the eight primitives are classes
//! too (ids 0–9), which keeps the type a single integer comparison in hot paths.

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ClassId(pub u16);

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Ty {
    pub class: ClassId,
    pub dims: u8,
}

impl Ty {
    pub const fn of(class: ClassId) -> Ty {
        Ty { class, dims: 0 }
    }
    pub const fn array(class: ClassId, dims: u8) -> Ty {
        Ty { class, dims }
    }
    pub const VOID: Ty = Ty::of(cls::VOID);
    pub const DEF: Ty = Ty::of(cls::DEF);
    pub const BOOLEAN: Ty = Ty::of(cls::BOOLEAN);
    pub const BYTE: Ty = Ty::of(cls::BYTE);
    pub const SHORT: Ty = Ty::of(cls::SHORT);
    pub const CHAR: Ty = Ty::of(cls::CHAR);
    pub const INT: Ty = Ty::of(cls::INT);
    pub const LONG: Ty = Ty::of(cls::LONG);
    pub const FLOAT: Ty = Ty::of(cls::FLOAT);
    pub const DOUBLE: Ty = Ty::of(cls::DOUBLE);
    pub const OBJECT: Ty = Ty::of(cls::OBJECT);
    pub const STRING: Ty = Ty::of(cls::STRING);

    pub fn is_primitive(self) -> bool {
        self.dims == 0 && (2..=9).contains(&self.class.0)
    }
    pub fn is_def(self) -> bool {
        self == Ty::DEF
    }
    pub fn is_void(self) -> bool {
        self == Ty::VOID
    }
    pub fn is_array(self) -> bool {
        self.dims > 0
    }
    pub fn is_numeric_primitive(self) -> bool {
        self.dims == 0 && (3..=9).contains(&self.class.0)
    }
    pub fn component(self) -> Ty {
        Ty { class: self.class, dims: self.dims.saturating_sub(1) }
    }
    pub fn boxed(self) -> Ty {
        if self.dims != 0 {
            return self;
        }
        Ty::of(match self.class {
            cls::BOOLEAN => cls::BOOLEAN_BOX,
            cls::BYTE => cls::BYTE_BOX,
            cls::SHORT => cls::SHORT_BOX,
            cls::CHAR => cls::CHARACTER,
            cls::INT => cls::INTEGER,
            cls::LONG => cls::LONG_BOX,
            cls::FLOAT => cls::FLOAT_BOX,
            cls::DOUBLE => cls::DOUBLE_BOX,
            c => c,
        })
    }
    pub fn unboxed(self) -> Ty {
        if self.dims != 0 {
            return self;
        }
        Ty::of(match self.class {
            cls::BOOLEAN_BOX => cls::BOOLEAN,
            cls::BYTE_BOX => cls::BYTE,
            cls::SHORT_BOX => cls::SHORT,
            cls::CHARACTER => cls::CHAR,
            cls::INTEGER => cls::INT,
            cls::LONG_BOX => cls::LONG,
            cls::FLOAT_BOX => cls::FLOAT,
            cls::DOUBLE_BOX => cls::DOUBLE,
            c => c,
        })
    }
}

macro_rules! classes {
    ($( $id:ident = $java:literal $(as $short:literal)? ;)*) => {
        #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
        #[repr(u16)]
        enum Idx { $($id,)* Count }
        // some classes exist only as names scripts can use (types, class literals)
        $(#[allow(dead_code)] pub const $id: ClassId = ClassId(Idx::$id as u16);)*
        pub const BUILTIN_COUNT: usize = Idx::Count as usize;
        /// Java canonical name and imported (short) name of every builtin class, by id.
        pub static BUILTIN: &[(&str, Option<&str>)] = &[ $( ($java, classes!(@short $($short)?)) ,)* ];
    };
    (@short) => { None };
    (@short $s:literal) => { Some($s) };
}

/// Builtin class ids. The order is the id; names are Java canonical names, with the short name
/// Painless imports them under.
pub mod cls {
    use super::ClassId;
    classes! {
        VOID = "void" as "void";
        DEF = "def" as "def";
        BOOLEAN = "boolean" as "boolean";
        BYTE = "byte" as "byte";
        SHORT = "short" as "short";
        CHAR = "char" as "char";
        INT = "int" as "int";
        LONG = "long" as "long";
        FLOAT = "float" as "float";
        DOUBLE = "double" as "double";
        // java.lang
        OBJECT = "java.lang.Object" as "Object";
        NUMBER = "java.lang.Number" as "Number";
        CHAR_SEQUENCE = "java.lang.CharSequence" as "CharSequence";
        COMPARABLE = "java.lang.Comparable" as "Comparable";
        ITERABLE = "java.lang.Iterable" as "Iterable";
        APPENDABLE = "java.lang.Appendable" as "Appendable";
        STRING = "java.lang.String" as "String";
        BOOLEAN_BOX = "java.lang.Boolean" as "Boolean";
        BYTE_BOX = "java.lang.Byte" as "Byte";
        SHORT_BOX = "java.lang.Short" as "Short";
        CHARACTER = "java.lang.Character" as "Character";
        INTEGER = "java.lang.Integer" as "Integer";
        LONG_BOX = "java.lang.Long" as "Long";
        FLOAT_BOX = "java.lang.Float" as "Float";
        DOUBLE_BOX = "java.lang.Double" as "Double";
        MATH = "java.lang.Math" as "Math";
        STRICT_MATH = "java.lang.StrictMath" as "StrictMath";
        STRING_BUILDER = "java.lang.StringBuilder" as "StringBuilder";
        STRING_BUFFER = "java.lang.StringBuffer" as "StringBuffer";
        SYSTEM = "java.lang.System" as "System";
        ENUM = "java.lang.Enum" as "Enum";
        VOID_BOX = "java.lang.Void" as "Void";
        STACK_TRACE_ELEMENT = "java.lang.StackTraceElement" as "StackTraceElement";
        // exceptions
        THROWABLE = "java.lang.Throwable";
        EXCEPTION = "java.lang.Exception" as "Exception";
        RUNTIME_EXCEPTION = "java.lang.RuntimeException" as "RuntimeException";
        ARITHMETIC_EXCEPTION = "java.lang.ArithmeticException" as "ArithmeticException";
        WRONG_METHOD_TYPE_EXCEPTION = "java.lang.invoke.WrongMethodTypeException";
        LAMBDA_CONVERSION_EXCEPTION = "java.lang.invoke.LambdaConversionException";
        INDEX_OUT_OF_BOUNDS_EXCEPTION = "java.lang.IndexOutOfBoundsException" as "IndexOutOfBoundsException";
        ARRAY_INDEX_OUT_OF_BOUNDS_EXCEPTION = "java.lang.ArrayIndexOutOfBoundsException" as "ArrayIndexOutOfBoundsException";
        STRING_INDEX_OUT_OF_BOUNDS_EXCEPTION = "java.lang.StringIndexOutOfBoundsException" as "StringIndexOutOfBoundsException";
        ARRAY_STORE_EXCEPTION = "java.lang.ArrayStoreException" as "ArrayStoreException";
        CLASS_CAST_EXCEPTION = "java.lang.ClassCastException" as "ClassCastException";
        REFLECTIVE_OPERATION_EXCEPTION = "java.lang.ReflectiveOperationException" as "ReflectiveOperationException";
        CLASS_NOT_FOUND_EXCEPTION = "java.lang.ClassNotFoundException" as "ClassNotFoundException";
        CLONE_NOT_SUPPORTED_EXCEPTION = "java.lang.CloneNotSupportedException" as "CloneNotSupportedException";
        ILLEGAL_ACCESS_EXCEPTION = "java.lang.IllegalAccessException" as "IllegalAccessException";
        ILLEGAL_ARGUMENT_EXCEPTION = "java.lang.IllegalArgumentException" as "IllegalArgumentException";
        NUMBER_FORMAT_EXCEPTION = "java.lang.NumberFormatException" as "NumberFormatException";
        ILLEGAL_MONITOR_STATE_EXCEPTION = "java.lang.IllegalMonitorStateException" as "IllegalMonitorStateException";
        ILLEGAL_STATE_EXCEPTION = "java.lang.IllegalStateException" as "IllegalStateException";
        ILLEGAL_THREAD_STATE_EXCEPTION = "java.lang.IllegalThreadStateException" as "IllegalThreadStateException";
        INSTANTIATION_EXCEPTION = "java.lang.InstantiationException" as "InstantiationException";
        INTERRUPTED_EXCEPTION = "java.lang.InterruptedException" as "InterruptedException";
        NEGATIVE_ARRAY_SIZE_EXCEPTION = "java.lang.NegativeArraySizeException" as "NegativeArraySizeException";
        NO_SUCH_FIELD_EXCEPTION = "java.lang.NoSuchFieldException" as "NoSuchFieldException";
        NO_SUCH_METHOD_EXCEPTION = "java.lang.NoSuchMethodException" as "NoSuchMethodException";
        NULL_POINTER_EXCEPTION = "java.lang.NullPointerException" as "NullPointerException";
        SECURITY_EXCEPTION = "java.lang.SecurityException" as "SecurityException";
        UNSUPPORTED_OPERATION_EXCEPTION = "java.lang.UnsupportedOperationException" as "UnsupportedOperationException";
        CONCURRENT_MODIFICATION_EXCEPTION = "java.util.ConcurrentModificationException" as "ConcurrentModificationException";
        NO_SUCH_ELEMENT_EXCEPTION = "java.util.NoSuchElementException" as "NoSuchElementException";
        EMPTY_STACK_EXCEPTION = "java.util.EmptyStackException" as "EmptyStackException";
        DATE_TIME_EXCEPTION = "java.time.DateTimeException" as "DateTimeException";
        DATE_TIME_PARSE_EXCEPTION = "java.time.format.DateTimeParseException" as "DateTimeParseException";
        UNSUPPORTED_TEMPORAL_TYPE_EXCEPTION = "java.time.temporal.UnsupportedTemporalTypeException" as "UnsupportedTemporalTypeException";
        PATTERN_SYNTAX_EXCEPTION = "java.util.regex.PatternSyntaxException";
        ENUM_CONSTANT_NOT_PRESENT_EXCEPTION = "java.lang.EnumConstantNotPresentException" as "EnumConstantNotPresentException";
        TYPE_NOT_PRESENT_EXCEPTION = "java.lang.TypeNotPresentException" as "TypeNotPresentException";
        ILLEGAL_FORMAT_EXCEPTION = "java.util.IllegalFormatException" as "IllegalFormatException";
        ILLEGAL_FORMAT_ARGUMENT_INDEX_EXCEPTION = "java.util.IllegalFormatArgumentIndexException";
        DUPLICATE_FORMAT_FLAGS_EXCEPTION = "java.util.DuplicateFormatFlagsException" as "DuplicateFormatFlagsException";
        FORMAT_FLAGS_CONVERSION_MISMATCH_EXCEPTION = "java.util.FormatFlagsConversionMismatchException" as "FormatFlagsConversionMismatchException";
        FORMATTER_CLOSED_EXCEPTION = "java.util.FormatterClosedException" as "FormatterClosedException";
        ILLEGAL_FORMAT_CODE_POINT_EXCEPTION = "java.util.IllegalFormatCodePointException" as "IllegalFormatCodePointException";
        ILLEGAL_FORMAT_CONVERSION_EXCEPTION = "java.util.IllegalFormatConversionException" as "IllegalFormatConversionException";
        ILLEGAL_FORMAT_FLAGS_EXCEPTION = "java.util.IllegalFormatFlagsException" as "IllegalFormatFlagsException";
        ILLEGAL_FORMAT_PRECISION_EXCEPTION = "java.util.IllegalFormatPrecisionException" as "IllegalFormatPrecisionException";
        ILLEGAL_FORMAT_WIDTH_EXCEPTION = "java.util.IllegalFormatWidthException" as "IllegalFormatWidthException";
        ILLFORMED_LOCALE_EXCEPTION = "java.util.IllformedLocaleException" as "IllformedLocaleException";
        INPUT_MISMATCH_EXCEPTION = "java.util.InputMismatchException" as "InputMismatchException";
        MISSING_FORMAT_ARGUMENT_EXCEPTION = "java.util.MissingFormatArgumentException" as "MissingFormatArgumentException";
        MISSING_FORMAT_WIDTH_EXCEPTION = "java.util.MissingFormatWidthException" as "MissingFormatWidthException";
        MISSING_RESOURCE_EXCEPTION = "java.util.MissingResourceException" as "MissingResourceException";
        TOO_MANY_LISTENERS_EXCEPTION = "java.util.TooManyListenersException" as "TooManyListenersException";
        UNKNOWN_FORMAT_CONVERSION_EXCEPTION = "java.util.UnknownFormatConversionException" as "UnknownFormatConversionException";
        UNKNOWN_FORMAT_FLAGS_EXCEPTION = "java.util.UnknownFormatFlagsException" as "UnknownFormatFlagsException";
        TEXT_PARSE_EXCEPTION = "java.text.ParseException" as "ParseException";
        FORMAT = "java.text.Format" as "Format";
        FORMAT_FIELD = "java.text.Format.Field" as "Format.Field";
        NUMBER_FORMAT = "java.text.NumberFormat" as "NumberFormat";
        NUMBER_FORMAT_FIELD = "java.text.NumberFormat.Field" as "NumberFormat.Field";
        DECIMAL_FORMAT = "java.text.DecimalFormat" as "DecimalFormat";
        DECIMAL_FORMAT_SYMBOLS = "java.text.DecimalFormatSymbols" as "DecimalFormatSymbols";
        PARSE_POSITION = "java.text.ParsePosition" as "ParsePosition";
        FIELD_POSITION = "java.text.FieldPosition" as "FieldPosition";
        NORMALIZER = "java.text.Normalizer" as "Normalizer";
        NORMALIZER_FORM = "java.text.Normalizer.Form" as "Normalizer.Form";
        CURRENCY = "java.util.Currency" as "Currency";
        ZONE_RULES_EXCEPTION = "java.time.zone.ZoneRulesException" as "ZoneRulesException";
        // java.util interfaces
        COLLECTION = "java.util.Collection" as "Collection";
        LIST = "java.util.List" as "List";
        SET = "java.util.Set" as "Set";
        SORTED_SET = "java.util.SortedSet" as "SortedSet";
        NAVIGABLE_SET = "java.util.NavigableSet" as "NavigableSet";
        QUEUE = "java.util.Queue" as "Queue";
        DEQUE = "java.util.Deque" as "Deque";
        MAP = "java.util.Map" as "Map";
        SORTED_MAP = "java.util.SortedMap" as "SortedMap";
        NAVIGABLE_MAP = "java.util.NavigableMap" as "NavigableMap";
        MAP_ENTRY = "java.util.Map.Entry" as "Map.Entry";
        ITERATOR = "java.util.Iterator" as "Iterator";
        LIST_ITERATOR = "java.util.ListIterator" as "ListIterator";
        PRIMITIVE_ITERATOR = "java.util.PrimitiveIterator" as "PrimitiveIterator";
        PRIMITIVE_ITERATOR_OF_INT = "java.util.PrimitiveIterator.OfInt" as "PrimitiveIterator.OfInt";
        PRIMITIVE_ITERATOR_OF_LONG = "java.util.PrimitiveIterator.OfLong" as "PrimitiveIterator.OfLong";
        PRIMITIVE_ITERATOR_OF_DOUBLE = "java.util.PrimitiveIterator.OfDouble" as "PrimitiveIterator.OfDouble";
        RANDOM_ACCESS = "java.util.RandomAccess" as "RandomAccess";
        COMPARATOR = "java.util.Comparator" as "Comparator";
        // java.util classes
        ABSTRACT_COLLECTION = "java.util.AbstractCollection" as "AbstractCollection";
        ABSTRACT_LIST = "java.util.AbstractList" as "AbstractList";
        ABSTRACT_SEQUENTIAL_LIST = "java.util.AbstractSequentialList" as "AbstractSequentialList";
        ABSTRACT_SET = "java.util.AbstractSet" as "AbstractSet";
        ABSTRACT_QUEUE = "java.util.AbstractQueue" as "AbstractQueue";
        ABSTRACT_MAP = "java.util.AbstractMap" as "AbstractMap";
        SIMPLE_ENTRY = "java.util.AbstractMap.SimpleEntry" as "AbstractMap.SimpleEntry";
        SIMPLE_IMMUTABLE_ENTRY = "java.util.AbstractMap.SimpleImmutableEntry" as "AbstractMap.SimpleImmutableEntry";
        ARRAY_LIST = "java.util.ArrayList" as "ArrayList";
        LINKED_LIST = "java.util.LinkedList" as "LinkedList";
        VECTOR = "java.util.Vector" as "Vector";
        STACK = "java.util.Stack" as "Stack";
        ARRAY_DEQUE = "java.util.ArrayDeque" as "ArrayDeque";
        PRIORITY_QUEUE = "java.util.PriorityQueue" as "PriorityQueue";
        HASH_MAP = "java.util.HashMap" as "HashMap";
        LINKED_HASH_MAP = "java.util.LinkedHashMap" as "LinkedHashMap";
        TREE_MAP = "java.util.TreeMap" as "TreeMap";
        HASHTABLE = "java.util.Hashtable" as "Hashtable";
        IDENTITY_HASH_MAP = "java.util.IdentityHashMap" as "IdentityHashMap";
        DICTIONARY = "java.util.Dictionary" as "Dictionary";
        HASH_SET = "java.util.HashSet" as "HashSet";
        LINKED_HASH_SET = "java.util.LinkedHashSet" as "LinkedHashSet";
        TREE_SET = "java.util.TreeSet" as "TreeSet";
        COLLECTIONS = "java.util.Collections" as "Collections";
        ARRAYS = "java.util.Arrays" as "Arrays";
        OBJECTS = "java.util.Objects" as "Objects";
        OPTIONAL = "java.util.Optional" as "Optional";
        OPTIONAL_INT = "java.util.OptionalInt" as "OptionalInt";
        OPTIONAL_LONG = "java.util.OptionalLong" as "OptionalLong";
        OPTIONAL_DOUBLE = "java.util.OptionalDouble" as "OptionalDouble";
        RANDOM = "java.util.Random" as "Random";
        STRING_JOINER = "java.util.StringJoiner" as "StringJoiner";
        STRING_TOKENIZER = "java.util.StringTokenizer" as "StringTokenizer";
        UUID = "java.util.UUID" as "UUID";
        BASE64 = "java.util.Base64" as "Base64";
        BASE64_ENCODER = "java.util.Base64.Encoder" as "Base64.Encoder";
        BASE64_DECODER = "java.util.Base64.Decoder" as "Base64.Decoder";
        INT_SUMMARY_STATISTICS = "java.util.IntSummaryStatistics" as "IntSummaryStatistics";
        LONG_SUMMARY_STATISTICS = "java.util.LongSummaryStatistics" as "LongSummaryStatistics";
        DOUBLE_SUMMARY_STATISTICS = "java.util.DoubleSummaryStatistics" as "DoubleSummaryStatistics";
        LOCALE = "java.util.Locale" as "Locale";
        BIT_SET = "java.util.BitSet" as "BitSet";
        DATE = "java.util.Date" as "Date";
        HEX_FORMAT = "java.util.HexFormat" as "HexFormat";
        // runtime-only classes of library objects (never named in scripts)
        ARRAYS_LIST = "java.util.Arrays.ArrayList";
        IMMUTABLE_LIST = "java.util.ImmutableCollections.ListN";
        IMMUTABLE_SET = "java.util.ImmutableCollections.SetN";
        IMMUTABLE_MAP = "java.util.ImmutableCollections.MapN";
        UNMODIFIABLE_LIST = "java.util.Collections.UnmodifiableRandomAccessList";
        UNMODIFIABLE_SET = "java.util.Collections.UnmodifiableSet";
        UNMODIFIABLE_MAP = "java.util.Collections.UnmodifiableMap";
        UNMODIFIABLE_COLLECTION = "java.util.Collections.UnmodifiableCollection";
        EMPTY_LIST = "java.util.Collections.EmptyList";
        EMPTY_MAP = "java.util.Collections.EmptyMap";
        EMPTY_SET = "java.util.Collections.EmptySet";
        SINGLETON_LIST = "java.util.Collections.SingletonList";
        SINGLETON_SET = "java.util.Collections.SingletonSet";
        SINGLETON_MAP = "java.util.Collections.SingletonMap";
        SUB_LIST = "java.util.ArrayList.SubList";
        ABSTRACT_SUB_LIST = "java.util.AbstractList.SubList";
        HASH_MAP_KEY_SET = "java.util.HashMap.KeySet";
        HASH_MAP_VALUES = "java.util.HashMap.Values";
        HASH_MAP_ENTRY_SET = "java.util.HashMap.EntrySet";
        HASH_MAP_NODE = "java.util.HashMap.Node";
        LINKED_KEY_SET = "java.util.LinkedHashMap.LinkedKeySet";
        LINKED_VALUES = "java.util.LinkedHashMap.LinkedValues";
        LINKED_ENTRY_SET = "java.util.LinkedHashMap.LinkedEntrySet";
        LINKED_ENTRY = "java.util.LinkedHashMap.Entry";
        TREE_MAP_KEY_SET = "java.util.TreeMap.KeySet";
        TREE_MAP_VALUES = "java.util.TreeMap.Values";
        TREE_MAP_ENTRY_SET = "java.util.TreeMap.EntrySet";
        TREE_MAP_ENTRY = "java.util.TreeMap.Entry";
        ARRAY_LIST_ITR = "java.util.ArrayList.Itr";
        LIST_ITR = "java.util.ArrayList.ListItr";
        HASH_MAP_ITERATOR = "java.util.HashMap.HashIterator";
        GENERIC_ITERATOR = "java.util.Iterator.Impl";
        PRIMITIVE_INT_ITERATOR = "java.util.stream.IntPipeline.Iterator";
        PRIMITIVE_LONG_ITERATOR = "java.util.stream.LongPipeline.Iterator";
        PRIMITIVE_DOUBLE_ITERATOR = "java.util.stream.DoublePipeline.Iterator";
        // java.util.function
        BI_CONSUMER = "java.util.function.BiConsumer" as "BiConsumer";
        BI_FUNCTION = "java.util.function.BiFunction" as "BiFunction";
        BINARY_OPERATOR = "java.util.function.BinaryOperator" as "BinaryOperator";
        BI_PREDICATE = "java.util.function.BiPredicate" as "BiPredicate";
        BOOLEAN_SUPPLIER = "java.util.function.BooleanSupplier" as "BooleanSupplier";
        CONSUMER = "java.util.function.Consumer" as "Consumer";
        DOUBLE_BINARY_OPERATOR = "java.util.function.DoubleBinaryOperator" as "DoubleBinaryOperator";
        DOUBLE_CONSUMER = "java.util.function.DoubleConsumer" as "DoubleConsumer";
        DOUBLE_FUNCTION = "java.util.function.DoubleFunction" as "DoubleFunction";
        DOUBLE_PREDICATE = "java.util.function.DoublePredicate" as "DoublePredicate";
        DOUBLE_SUPPLIER = "java.util.function.DoubleSupplier" as "DoubleSupplier";
        DOUBLE_TO_INT_FUNCTION = "java.util.function.DoubleToIntFunction" as "DoubleToIntFunction";
        DOUBLE_TO_LONG_FUNCTION = "java.util.function.DoubleToLongFunction" as "DoubleToLongFunction";
        DOUBLE_UNARY_OPERATOR = "java.util.function.DoubleUnaryOperator" as "DoubleUnaryOperator";
        FUNCTION = "java.util.function.Function" as "Function";
        INT_BINARY_OPERATOR = "java.util.function.IntBinaryOperator" as "IntBinaryOperator";
        INT_CONSUMER = "java.util.function.IntConsumer" as "IntConsumer";
        INT_FUNCTION = "java.util.function.IntFunction" as "IntFunction";
        INT_PREDICATE = "java.util.function.IntPredicate" as "IntPredicate";
        INT_SUPPLIER = "java.util.function.IntSupplier" as "IntSupplier";
        INT_TO_DOUBLE_FUNCTION = "java.util.function.IntToDoubleFunction" as "IntToDoubleFunction";
        INT_TO_LONG_FUNCTION = "java.util.function.IntToLongFunction" as "IntToLongFunction";
        INT_UNARY_OPERATOR = "java.util.function.IntUnaryOperator" as "IntUnaryOperator";
        LONG_BINARY_OPERATOR = "java.util.function.LongBinaryOperator" as "LongBinaryOperator";
        LONG_CONSUMER = "java.util.function.LongConsumer" as "LongConsumer";
        LONG_FUNCTION = "java.util.function.LongFunction" as "LongFunction";
        LONG_PREDICATE = "java.util.function.LongPredicate" as "LongPredicate";
        LONG_SUPPLIER = "java.util.function.LongSupplier" as "LongSupplier";
        LONG_TO_DOUBLE_FUNCTION = "java.util.function.LongToDoubleFunction" as "LongToDoubleFunction";
        LONG_TO_INT_FUNCTION = "java.util.function.LongToIntFunction" as "LongToIntFunction";
        LONG_UNARY_OPERATOR = "java.util.function.LongUnaryOperator" as "LongUnaryOperator";
        OBJ_DOUBLE_CONSUMER = "java.util.function.ObjDoubleConsumer" as "ObjDoubleConsumer";
        OBJ_INT_CONSUMER = "java.util.function.ObjIntConsumer" as "ObjIntConsumer";
        OBJ_LONG_CONSUMER = "java.util.function.ObjLongConsumer" as "ObjLongConsumer";
        PREDICATE = "java.util.function.Predicate" as "Predicate";
        SUPPLIER = "java.util.function.Supplier" as "Supplier";
        TO_DOUBLE_BI_FUNCTION = "java.util.function.ToDoubleBiFunction" as "ToDoubleBiFunction";
        TO_DOUBLE_FUNCTION = "java.util.function.ToDoubleFunction" as "ToDoubleFunction";
        TO_INT_BI_FUNCTION = "java.util.function.ToIntBiFunction" as "ToIntBiFunction";
        TO_INT_FUNCTION = "java.util.function.ToIntFunction" as "ToIntFunction";
        TO_LONG_BI_FUNCTION = "java.util.function.ToLongBiFunction" as "ToLongBiFunction";
        TO_LONG_FUNCTION = "java.util.function.ToLongFunction" as "ToLongFunction";
        UNARY_OPERATOR = "java.util.function.UnaryOperator" as "UnaryOperator";
        LAMBDA = "org.elasticsearch.painless.Lambda";
        // java.util.regex
        PATTERN = "java.util.regex.Pattern" as "Pattern";
        MATCHER = "java.util.regex.Matcher" as "Matcher";
        // java.util.stream
        BASE_STREAM = "java.util.stream.BaseStream" as "BaseStream";
        STREAM = "java.util.stream.Stream" as "Stream";
        INT_STREAM = "java.util.stream.IntStream" as "IntStream";
        LONG_STREAM = "java.util.stream.LongStream" as "LongStream";
        DOUBLE_STREAM = "java.util.stream.DoubleStream" as "DoubleStream";
        STREAM_BUILDER = "java.util.stream.Stream.Builder" as "Stream.Builder";
        INT_STREAM_BUILDER = "java.util.stream.IntStream.Builder" as "IntStream.Builder";
        LONG_STREAM_BUILDER = "java.util.stream.LongStream.Builder" as "LongStream.Builder";
        DOUBLE_STREAM_BUILDER = "java.util.stream.DoubleStream.Builder" as "DoubleStream.Builder";
        COLLECTOR_CHARACTERISTICS = "java.util.stream.Collector.Characteristics" as "Collector.Characteristics";
        STREAM_BUILDER_IMPL = "java.util.stream.Streams.StreamBuilderImpl";
        INT_STREAM_BUILDER_IMPL = "java.util.stream.Streams.IntStreamBuilderImpl";
        LONG_STREAM_BUILDER_IMPL = "java.util.stream.Streams.LongStreamBuilderImpl";
        DOUBLE_STREAM_BUILDER_IMPL = "java.util.stream.Streams.DoubleStreamBuilderImpl";
        COLLECTOR = "java.util.stream.Collector" as "Collector";
        COLLECTORS = "java.util.stream.Collectors" as "Collectors";
        REFERENCE_PIPELINE = "java.util.stream.ReferencePipeline";
        INT_PIPELINE = "java.util.stream.IntPipeline";
        LONG_PIPELINE = "java.util.stream.LongPipeline";
        DOUBLE_PIPELINE = "java.util.stream.DoublePipeline";
        COLLECTOR_IMPL = "java.util.stream.Collectors.CollectorImpl";
        // java.time
        TEMPORAL_ACCESSOR = "java.time.temporal.TemporalAccessor" as "TemporalAccessor";
        TEMPORAL = "java.time.temporal.Temporal" as "Temporal";
        TEMPORAL_ADJUSTER = "java.time.temporal.TemporalAdjuster" as "TemporalAdjuster";
        TEMPORAL_AMOUNT = "java.time.temporal.TemporalAmount" as "TemporalAmount";
        TEMPORAL_UNIT = "java.time.temporal.TemporalUnit" as "TemporalUnit";
        TEMPORAL_FIELD = "java.time.temporal.TemporalField" as "TemporalField";
        CHRONO_UNIT = "java.time.temporal.ChronoUnit" as "ChronoUnit";
        CHRONO_FIELD = "java.time.temporal.ChronoField" as "ChronoField";
        TEMPORAL_ADJUSTERS = "java.time.temporal.TemporalAdjusters" as "TemporalAdjusters";
        CHRONO_ZONED_DATE_TIME = "java.time.chrono.ChronoZonedDateTime" as "ChronoZonedDateTime";
        CHRONO_LOCAL_DATE_TIME = "java.time.chrono.ChronoLocalDateTime" as "ChronoLocalDateTime";
        CHRONO_LOCAL_DATE = "java.time.chrono.ChronoLocalDate" as "ChronoLocalDate";
        CHRONO_PERIOD = "java.time.chrono.ChronoPeriod" as "ChronoPeriod";
        INSTANT = "java.time.Instant" as "Instant";
        DURATION = "java.time.Duration" as "Duration";
        PERIOD = "java.time.Period" as "Period";
        LOCAL_DATE = "java.time.LocalDate" as "LocalDate";
        LOCAL_TIME = "java.time.LocalTime" as "LocalTime";
        LOCAL_DATE_TIME = "java.time.LocalDateTime" as "LocalDateTime";
        ZONED_DATE_TIME = "java.time.ZonedDateTime" as "ZonedDateTime";
        OFFSET_DATE_TIME = "java.time.OffsetDateTime" as "OffsetDateTime";
        ZONE_ID = "java.time.ZoneId" as "ZoneId";
        ZONE_OFFSET = "java.time.ZoneOffset" as "ZoneOffset";
        ZONE_REGION = "java.time.ZoneRegion";
        DAY_OF_WEEK = "java.time.DayOfWeek" as "DayOfWeek";
        MONTH = "java.time.Month" as "Month";
        YEAR = "java.time.Year" as "Year";
        YEAR_MONTH = "java.time.YearMonth" as "YearMonth";
        MONTH_DAY = "java.time.MonthDay" as "MonthDay";
        DATE_TIME_FORMATTER = "java.time.format.DateTimeFormatter" as "DateTimeFormatter";
        TEXT_STYLE = "java.time.format.TextStyle" as "TextStyle";
        RESOLVER_STYLE = "java.time.format.ResolverStyle" as "ResolverStyle";
        FORMAT_STYLE = "java.time.format.FormatStyle" as "FormatStyle";
        SIGN_STYLE = "java.time.format.SignStyle" as "SignStyle";
        OFFSET_TIME = "java.time.OffsetTime" as "OffsetTime";
        CLOCK = "java.time.Clock" as "Clock";
        VALUE_RANGE = "java.time.temporal.ValueRange" as "ValueRange";
        TEMPORAL_QUERY = "java.time.temporal.TemporalQuery" as "TemporalQuery";
        ISO_FIELDS = "java.time.temporal.IsoFields" as "IsoFields";
        JULIAN_FIELDS = "java.time.temporal.JulianFields" as "JulianFields";
        WEEK_FIELDS = "java.time.temporal.WeekFields" as "WeekFields";
        ISO_CHRONOLOGY = "java.time.chrono.IsoChronology" as "IsoChronology";
        CHRONOLOGY = "java.time.chrono.Chronology" as "Chronology";
        TEMPORAL_ADJUSTER_IMPL = "java.time.temporal.TemporalAdjusters.Impl";
        // java.math
        BIG_INTEGER = "java.math.BigInteger" as "BigInteger";
        BIG_DECIMAL = "java.math.BigDecimal" as "BigDecimal";
        MATH_CONTEXT = "java.math.MathContext" as "MathContext";
        ROUNDING_MODE = "java.math.RoundingMode" as "RoundingMode";
        // Elasticsearch
        DEBUG = "org.elasticsearch.painless.api.Debug" as "Debug";
        JSON = "org.elasticsearch.painless.api.Json" as "Json";
        CIDR = "org.elasticsearch.painless.api.CIDR" as "CIDR";
        GEO_POINT = "org.elasticsearch.common.geo.GeoPoint" as "GeoPoint";
        GEO_BOUNDING_BOX = "org.elasticsearch.common.geo.GeoBoundingBox" as "GeoBoundingBox";
        BYTES_REF = "org.apache.lucene.util.BytesRef" as "BytesRef";
        SCRIPT_DOC_VALUES = "org.elasticsearch.index.fielddata.ScriptDocValues" as "ScriptDocValues";
        SDV_STRINGS = "org.elasticsearch.index.fielddata.ScriptDocValues.Strings" as "ScriptDocValues.Strings";
        SDV_LONGS = "org.elasticsearch.index.fielddata.ScriptDocValues.Longs" as "ScriptDocValues.Longs";
        SDV_DATES = "org.elasticsearch.index.fielddata.ScriptDocValues.Dates" as "ScriptDocValues.Dates";
        SDV_DOUBLES = "org.elasticsearch.index.fielddata.ScriptDocValues.Doubles" as "ScriptDocValues.Doubles";
        SDV_GEOMETRY = "org.elasticsearch.index.fielddata.ScriptDocValues.Geometry" as "ScriptDocValues.Geometry";
        SDV_GEO_POINTS = "org.elasticsearch.index.fielddata.ScriptDocValues.GeoPoints" as "ScriptDocValues.GeoPoints";
        SDV_BOOLEANS = "org.elasticsearch.index.fielddata.ScriptDocValues.Booleans" as "ScriptDocValues.Booleans";
        SDV_BYTES_REFS = "org.elasticsearch.index.fielddata.ScriptDocValues.BytesRefs" as "ScriptDocValues.BytesRefs";
        SDV_IP = "org.elasticsearch.index.mapper.IpFieldMapper.IpFieldType.IpScriptDocValues" as "IpFieldMapper.IpFieldType.IpScriptDocValues";
        SDV_DENSE_VECTOR = "org.elasticsearch.index.mapper.vectors.DenseVectorScriptDocValues" as "DenseVectorScriptDocValues";
        FIELD_LOOKUP = "org.elasticsearch.search.lookup.FieldLookup" as "FieldLookup";
        DOC_MAP = "org.elasticsearch.search.lookup.LeafDocLookup";
        SOURCE_MAP = "org.elasticsearch.search.lookup.SourceLookup";
        FIELD = "org.elasticsearch.script.field.Field" as "Field";
        DOC_VALUES_FIELD = "org.elasticsearch.script.field.DocValuesField";
        EMPTY_FIELD = "org.elasticsearch.script.field.EmptyField" as "EmptyField";
        DENSE_VECTOR = "org.elasticsearch.script.field.vectors.DenseVector" as "DenseVector";
        WRITE_FIELD = "org.elasticsearch.script.field.WriteField" as "WriteField";
        METADATA = "org.elasticsearch.script.Metadata" as "Metadata";
        EXPLANATION_HOLDER = "org.elasticsearch.script.ScoreScript.ExplanationHolder" as "ScoreScript.ExplanationHolder";
        SCRIPT_TERM_STATS = "org.elasticsearch.script.ScriptTermStats" as "ScriptTermStats";
        STATS_SUMMARY = "org.elasticsearch.script.StatsSummary" as "StatsSummary";
        SIMILARITY_QUERY = "org.elasticsearch.index.similarity.ScriptedSimilarity.Query" as "ScriptedSimilarity.Query";
        SIMILARITY_FIELD = "org.elasticsearch.index.similarity.ScriptedSimilarity.Field" as "ScriptedSimilarity.Field";
        SIMILARITY_TERM = "org.elasticsearch.index.similarity.ScriptedSimilarity.Term" as "ScriptedSimilarity.Term";
        SIMILARITY_DOC = "org.elasticsearch.index.similarity.ScriptedSimilarity.Doc" as "ScriptedSimilarity.Doc";
        INTERVAL = "org.elasticsearch.index.query.IntervalFilterScript.Interval" as "IntervalFilterScript.Interval";
        ANALYSIS_TOKEN = "org.elasticsearch.analysis.common.AnalysisPredicateScript.Token" as "AnalysisPredicateScript.Token";
        PACKED_TOKEN_ATTRIBUTE = "org.apache.lucene.analysis.tokenattributes.PackedTokenAttributeImpl";
        LONG_ACCESSOR = "org.elasticsearch.search.aggregations.bucket.terms.heuristic.ScriptHeuristic.LongAccessor";
        HOST_OBJECT = "org.elasticsearch.script.HostObject";
        SDV_UNSIGNED_LONG = "org.elasticsearch.xpack.unsignedlong.UnsignedLongScriptDocValues";
        SDV_VERSION = "org.elasticsearch.xpack.versionfield.VersionScriptDocValues";
        IP_ADDRESS = "org.elasticsearch.script.field.IPAddress" as "IPAddress";
        DELEGATE_DOC_VALUES_FIELD = "org.elasticsearch.script.field.DelegateDocValuesField";
        BINARY_DOC_VALUES_FIELD = "org.elasticsearch.script.field.BinaryDocValuesField";
        BOOLEAN_DOC_VALUES_FIELD = "org.elasticsearch.script.field.BooleanDocValuesField";
        INTEGER_DOC_VALUES_FIELD = "org.elasticsearch.script.field.IntegerDocValuesField";
        ABSTRACT_LONG_DOC_VALUES_FIELD = "org.elasticsearch.script.field.AbstractLongDocValuesField";
        LONG_DOC_VALUES_FIELD = "org.elasticsearch.script.field.LongDocValuesField";
        DOUBLE_DOC_VALUES_FIELD = "org.elasticsearch.script.field.DoubleDocValuesField";
        FLOAT_DOC_VALUES_FIELD = "org.elasticsearch.script.field.FloatDocValuesField";
        SCALED_FLOAT_DOC_VALUES_FIELD = "org.elasticsearch.script.field.ScaledFloatDocValuesField";
        HALF_FLOAT_DOC_VALUES_FIELD = "org.elasticsearch.script.field.HalfFloatDocValuesField";
        BYTE_DOC_VALUES_FIELD = "org.elasticsearch.script.field.ByteDocValuesField";
        SHORT_DOC_VALUES_FIELD = "org.elasticsearch.script.field.ShortDocValuesField";
        DATE_MILLIS_DOC_VALUES_FIELD = "org.elasticsearch.script.field.DateMillisDocValuesField";
        DATE_NANOS_DOC_VALUES_FIELD = "org.elasticsearch.script.field.DateNanosDocValuesField";
        BASE_KEYWORD_DOC_VALUES_FIELD = "org.elasticsearch.script.field.BaseKeywordDocValuesField";
        KEYWORD_DOC_VALUES_FIELD = "org.elasticsearch.script.field.KeywordDocValuesField";
        FLATTENED_DOC_VALUES_FIELD = "org.elasticsearch.script.field.FlattenedDocValuesField";
        GEO_POINT_DOC_VALUES_FIELD = "org.elasticsearch.script.field.GeoPointDocValuesField";
        IP_DOC_VALUES_FIELD = "org.elasticsearch.script.field.IpDocValuesField";
        SEQ_NO_DOC_VALUES_FIELD = "org.elasticsearch.script.field.SeqNoDocValuesField";
        VERSION_DOC_VALUES_FIELD = "org.elasticsearch.script.field.VersionDocValuesField";
        UNSIGNED_LONG_DOC_VALUES_FIELD = "org.elasticsearch.xpack.unsignedlong.UnsignedLongDocValuesField";
        VERSION_STRING_DOC_VALUES_FIELD = "org.elasticsearch.xpack.versionfield.VersionStringDocValuesField";
        DENSE_VECTOR_DOC_VALUES_FIELD = "org.elasticsearch.script.field.vectors.DenseVectorDocValuesField";
        KNN_DENSE_VECTOR = "org.elasticsearch.script.field.vectors.KnnDenseVector";
        BINARY_DENSE_VECTOR = "org.elasticsearch.script.field.vectors.BinaryDenseVector";
        BYTE_KNN_DENSE_VECTOR = "org.elasticsearch.script.field.vectors.ByteKnnDenseVector";
        BYTE_BINARY_DENSE_VECTOR = "org.elasticsearch.script.field.vectors.ByteBinaryDenseVector";
        BIT_KNN_DENSE_VECTOR = "org.elasticsearch.script.field.vectors.BitKnnDenseVector";
        BIT_BINARY_DENSE_VECTOR = "org.elasticsearch.script.field.vectors.BitBinaryDenseVector";
        DENSE_VECTOR_EMPTY = "org.elasticsearch.script.field.vectors.DenseVector.Empty";
        RANK_VECTORS = "org.elasticsearch.script.field.vectors.RankVectors" as "RankVectors";
        RANK_VECTORS_EMPTY = "org.elasticsearch.script.field.vectors.RankVectors.Empty";
        FLOAT_RANK_VECTORS = "org.elasticsearch.script.field.vectors.FloatRankVectors";
        BFLOAT16_RANK_VECTORS = "org.elasticsearch.script.field.vectors.BFloat16RankVectors";
        BYTE_RANK_VECTORS = "org.elasticsearch.script.field.vectors.ByteRankVectors";
        BIT_RANK_VECTORS = "org.elasticsearch.script.field.vectors.BitRankVectors";
        RANK_VECTORS_DOC_VALUES_FIELD = "org.elasticsearch.script.field.vectors.RankVectorsDocValuesField";
        SDV_GEO_SHAPE = "org.elasticsearch.xpack.spatial.index.fielddata.plain.AbstractAtomicGeoShapeShapeFieldData.GeoShapeScriptValues";
        GEO_SHAPE_VALUE = "org.elasticsearch.xpack.spatial.index.fielddata.GeoShapeValues.GeoShapeValue";
        GEO_SHAPE_DOC_VALUES_FIELD = "org.elasticsearch.xpack.spatial.index.mapper.GeoShapeWithDocValuesFieldMapper.GeoShapeDocValuesField";
        SDV_RANK_VECTORS = "org.elasticsearch.index.mapper.vectors.RankVectorsScriptDocValues" as "RankVectorsScriptDocValues";
        NESTED_DOCUMENT = "org.elasticsearch.script.field.NestedDocument" as "NestedDocument";
        CTX_MAP = "org.elasticsearch.script.CtxMap";
        DOC_BASED_SCRIPT = "org.elasticsearch.script.DocBasedScript";
        SCORE_SCRIPT = "org.elasticsearch.script.ScoreScript";
        WRITE_SCRIPT = "org.elasticsearch.script.WriteScript";
        BOOLEAN_FIELD_SCRIPT = "org.elasticsearch.script.BooleanFieldScript";
        DATE_FIELD_SCRIPT = "org.elasticsearch.script.DateFieldScript";
        DOUBLE_FIELD_SCRIPT = "org.elasticsearch.script.DoubleFieldScript";
        GEO_POINT_FIELD_SCRIPT = "org.elasticsearch.script.GeoPointFieldScript";
        GEOMETRY_FIELD_SCRIPT = "org.elasticsearch.script.GeometryFieldScript";
        IP_FIELD_SCRIPT = "org.elasticsearch.script.IpFieldScript";
        STRING_FIELD_SCRIPT = "org.elasticsearch.script.StringFieldScript";
        LONG_FIELD_SCRIPT = "org.elasticsearch.script.LongFieldScript";
        COMPOSITE_FIELD_SCRIPT = "org.elasticsearch.script.CompositeFieldScript";
        BYTE_ORDER = "java.nio.ByteOrder" as "ByteOrder";
        NIO_BUFFER = "java.nio.Buffer" as "Buffer";
        BYTE_BUFFER = "java.nio.ByteBuffer" as "ByteBuffer";
        HEAP_BYTE_BUFFER = "java.nio.HeapByteBuffer";
        TIME_ZONE = "java.util.TimeZone" as "TimeZone";
        SIMPLE_TIME_ZONE = "java.util.SimpleTimeZone" as "SimpleTimeZone";
        ZONE_INFO = "sun.util.calendar.ZoneInfo";
        CALENDAR = "java.util.Calendar" as "Calendar";
        GREGORIAN_CALENDAR = "java.util.GregorianCalendar" as "GregorianCalendar";
        CALENDAR_BUILDER = "java.util.Calendar.Builder" as "Calendar.Builder";
        DATE_FORMAT = "java.text.DateFormat" as "DateFormat";
        MESSAGE_FORMAT = "java.text.MessageFormat" as "MessageFormat";
        MESSAGE_FORMAT_FIELD = "java.text.MessageFormat.Field" as "MessageFormat.Field";
        CHOICE_FORMAT = "java.text.ChoiceFormat" as "ChoiceFormat";
        SIMPLE_DATE_FORMAT = "java.text.SimpleDateFormat" as "SimpleDateFormat";
        DATE_FORMAT_SYMBOLS = "java.text.DateFormatSymbols" as "DateFormatSymbols";
        DATE_FORMAT_FIELD = "java.text.DateFormat.Field" as "DateFormat.Field";
        CHAR_BUFFER = "java.nio.CharBuffer" as "CharBuffer";
        SHORT_BUFFER = "java.nio.ShortBuffer" as "ShortBuffer";
        INT_BUFFER = "java.nio.IntBuffer" as "IntBuffer";
        LONG_BUFFER = "java.nio.LongBuffer" as "LongBuffer";
        FLOAT_BUFFER = "java.nio.FloatBuffer" as "FloatBuffer";
        DOUBLE_BUFFER = "java.nio.DoubleBuffer" as "DoubleBuffer";
        // views of a ByteBuffer (`asIntBuffer()`), big- and little-endian
        CHAR_BUFFER_B = "java.nio.ByteBufferAsCharBufferB";
        CHAR_BUFFER_L = "java.nio.ByteBufferAsCharBufferL";
        SHORT_BUFFER_B = "java.nio.ByteBufferAsShortBufferB";
        SHORT_BUFFER_L = "java.nio.ByteBufferAsShortBufferL";
        INT_BUFFER_B = "java.nio.ByteBufferAsIntBufferB";
        INT_BUFFER_L = "java.nio.ByteBufferAsIntBufferL";
        LONG_BUFFER_B = "java.nio.ByteBufferAsLongBufferB";
        LONG_BUFFER_L = "java.nio.ByteBufferAsLongBufferL";
        FLOAT_BUFFER_B = "java.nio.ByteBufferAsFloatBufferB";
        FLOAT_BUFFER_L = "java.nio.ByteBufferAsFloatBufferL";
        DOUBLE_BUFFER_B = "java.nio.ByteBufferAsDoubleBufferB";
        DOUBLE_BUFFER_L = "java.nio.ByteBufferAsDoubleBufferL";
        LEAF_STORED_FIELDS = "org.elasticsearch.search.lookup.LeafStoredFieldsLookup";
        // extension allowlists of single contexts (see `contexts::class_visible`)
        MOVING_FUNCTIONS = "org.elasticsearch.search.aggregations.pipeline.MovingFunctions" as "MovingFunctions";
        INGEST_PROCESSORS = "org.elasticsearch.ingest.common.Processors" as "Processors";
        // `@no_import`: only the canonical name resolves
        NAMED_GROUP_EXTRACTOR = "org.elasticsearch.runtimefields.NamedGroupExtractor" as "org.elasticsearch.runtimefields.NamedGroupExtractor";
        SOURCE_MAP_FIELD = "org.elasticsearch.script.field.SourceMapField" as "SourceMapField";
        INGEST_CONDITIONAL_SCRIPT = "org.elasticsearch.script.IngestConditionalScript";
        // `IngestDocument.getUnmodifiableCtxMap()`: the read-only view processor conditions get
        INGEST_DATA = "org.elasticsearch.ingest.IngestDocument.UnmodifiableIngestData";
        INGEST_LIST = "org.elasticsearch.ingest.IngestDocument.UnmodifiableIngestList";
        LOCALE_BUILDER = "java.util.Locale.Builder" as "Locale.Builder";
        LOCALE_CATEGORY = "java.util.Locale.Category" as "Locale.Category";
        ZONE_RULES = "java.time.zone.ZoneRules" as "ZoneRules";
        ZONE_OFFSET_TRANSITION = "java.time.zone.ZoneOffsetTransition" as "ZoneOffsetTransition";
        CHARACTER_SUBSET = "java.lang.Character.Subset" as "Character.Subset";
        UNICODE_BLOCK = "java.lang.Character.UnicodeBlock" as "Character.UnicodeBlock";
        UNICODE_SCRIPT = "java.lang.Character.UnicodeScript" as "Character.UnicodeScript";
        DATE_TIME_FORMATTER_BUILDER = "java.time.format.DateTimeFormatterBuilder" as "DateTimeFormatterBuilder";
    }
}
