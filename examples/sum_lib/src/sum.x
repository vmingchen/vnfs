struct sum_args {
    int a;
    int b; 
};

program SUMPROG {
    version SUMVERS {
        int sum(sum_args) = 1;
    } = 1;
} = 22888;
